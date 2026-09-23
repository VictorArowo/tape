use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const LOOKBACK_BARS: usize = 360;
const HISTORY_HOURS: usize = 720;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct MinuteSample {
    ts: i64,
    close: f64,
    dollar_volume: f64,
}

struct HourlyImpact {
    ts: i64,
    value: f64,
}

struct DownsideImpactGate {
    minutes: VecDeque<MinuteSample>,
    hourly: VecDeque<HourlyImpact>,
}

impl DownsideImpactGate {
    fn impact(&self) -> Option<f64> {
        if self.minutes.len() < LOOKBACK_BARS + 1 {
            return None;
        }

        let mut downside = 0.0;
        let mut dollar_volume = 0.0;
        for i in 1..self.minutes.len() {
            let previous = self.minutes[i - 1].close;
            let current = self.minutes[i].close;
            let volume = self.minutes[i].dollar_volume;
            if previous <= 0.0 || current <= 0.0 || volume < 0.0 {
                return None;
            }
            downside += (previous / current).ln().max(0.0);
            dollar_volume += volume;
        }

        if dollar_volume > 0.0 && downside.is_finite() && dollar_volume.is_finite() {
            Some(downside / dollar_volume)
        } else {
            None
        }
    }

    fn percentile(&self, p: f64) -> Option<f64> {
        if self.hourly.is_empty() {
            return None;
        }
        let mut values: Vec<f64> = self.hourly.iter().map(|sample| sample.value).collect();
        values.sort_by(|a, b| a.total_cmp(b));

        // Linear interpolation between adjacent ranked observations.
        let index = (values.len() - 1) as f64 * p;
        let low = index.floor() as usize;
        let high = index.ceil() as usize;
        let weight = index - low as f64;
        Some(values[low] * (1.0 - weight) + values[high] * weight)
    }

    fn buy(&self, ctx: &mut Ctx, bar: &Bar) {
        let volume_cap = bar.volume.mul_ratio(1, 10);
        if !volume_cap.is_positive() || !bar.close.is_positive() || ctx.open_orders() != 0 {
            return;
        }
        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        let qty = if affordable < volume_cap {
            affordable
        } else {
            volume_cap
        };
        if qty.is_positive() && ctx.open_orders() == 0 {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }

    fn sell(&self, ctx: &mut Ctx, bar: &Bar, position: Qty) {
        let volume_cap = bar.volume.mul_ratio(1, 10);
        if !volume_cap.is_positive() || ctx.open_orders() != 0 {
            return;
        }
        let qty = if position < volume_cap {
            position
        } else {
            volume_cap
        };
        if qty.is_positive() && ctx.open_orders() == 0 {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
        }
    }
}

impl Strategy for DownsideImpactGate {
    fn name(&self) -> String {
        "Downside Impact Gate".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let ts = bar.close_ts.as_nanos();
        if let Some(previous) = self.minutes.back() {
            if ts - previous.ts != minute_ns {
                self.minutes.clear();
            }
        }

        let close = bar.close.to_f64();
        let volume = bar.volume.to_f64();
        self.minutes.push_back(MinuteSample {
            ts,
            close,
            dollar_volume: close * volume,
        });
        while self.minutes.len() > LOOKBACK_BARS + 1 {
            self.minutes.pop_front();
        }

        let minute_of_hour = (ts / minute_ns) % 60;
        if minute_of_hour != 0 {
            return;
        }

        let cutoff = ts - TimeDelta::from_days(30).as_nanos();
        self.hourly.retain(|sample| sample.ts >= cutoff);

        let Some(current_impact) = self.impact() else {
            return;
        };

        // The current observation is deliberately excluded from both thresholds.
        if self.hourly.len() >= HISTORY_HOURS {
            let p40 = self.percentile(0.40);
            let p90 = self.percentile(0.90);
            if let (Some(p40), Some(p90)) = (p40, p90) {
                let position = ctx.position(INSTRUMENT);
                if position.is_zero() && current_impact <= p40 {
                    self.buy(ctx, bar);
                } else if position.is_positive() && current_impact >= p90 {
                    self.sell(ctx, bar, position);
                }
            }
        }

        self.hourly.push_back(HourlyImpact {
            ts,
            value: current_impact,
        });
        while self.hourly.len() > HISTORY_HOURS {
            self.hourly.pop_front();
        }
    }
}

export_strategy!(DownsideImpactGate {
    minutes: VecDeque::with_capacity(LOOKBACK_BARS + 1),
    hourly: VecDeque::with_capacity(HISTORY_HOURS + 1),
});
