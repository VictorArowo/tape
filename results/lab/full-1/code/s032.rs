use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const VWAP_BARS: usize = 12 * 60;
const RESIDUAL_BARS: usize = 7 * 24 * 60;
const VOLUME_WINDOW: usize = 60;
const PRIOR_VOLUME_WINDOWS: usize = 24 * 60;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct RollingVwap {
    window: VecDeque<(i64, i64)>,
    weighted_sum: i128,
    volume_sum: i128,
}

impl RollingVwap {
    fn new() -> Self {
        Self {
            window: VecDeque::with_capacity(VWAP_BARS + 1),
            weighted_sum: 0,
            volume_sum: 0,
        }
    }

    fn push(&mut self, bar: &Bar) -> Option<i64> {
        let typical = ((i128::from(bar.high.raw())
            + i128::from(bar.low.raw())
            + i128::from(bar.close.raw()))
            / 3) as i64;
        let volume = bar.volume.raw().max(0);

        self.window.push_back((typical, volume));
        self.weighted_sum += i128::from(typical) * i128::from(volume);
        self.volume_sum += i128::from(volume);

        if self.window.len() > VWAP_BARS {
            let (old_price, old_volume) = self.window.pop_front().expect("non-empty");
            self.weighted_sum -= i128::from(old_price) * i128::from(old_volume);
            self.volume_sum -= i128::from(old_volume);
        }

        if self.window.len() == VWAP_BARS && self.volume_sum > 0 {
            Some((self.weighted_sum / self.volume_sum) as i64)
        } else {
            None
        }
    }
}

struct RollingResidualStd {
    window: VecDeque<f64>,
    mean: f64,
    m2: f64,
}

impl RollingResidualStd {
    fn new() -> Self {
        Self {
            window: VecDeque::with_capacity(RESIDUAL_BARS + 1),
            mean: 0.0,
            m2: 0.0,
        }
    }

    fn push(&mut self, residual: i64) -> Option<f64> {
        if self.window.len() == RESIDUAL_BARS {
            let old = self.window.pop_front().expect("non-empty");
            let old_count = self.window.len() as f64 + 1.0;
            let new_count = old_count - 1.0;
            if new_count > 0.0 {
                let new_mean = (old_count * self.mean - old) / new_count;
                self.m2 -= (old - self.mean) * (old - new_mean);
                self.mean = new_mean;
                self.m2 = self.m2.max(0.0);
            } else {
                self.mean = 0.0;
                self.m2 = 0.0;
            }
        }

        let value = residual as f64;
        let new_count = self.window.len() as f64 + 1.0;
        let delta = value - self.mean;
        self.mean += delta / new_count;
        self.m2 += delta * (value - self.mean);
        self.window.push_back(value);

        if self.window.len() == RESIDUAL_BARS {
            let variance = (self.m2 / RESIDUAL_BARS as f64).max(0.0);
            let standard_deviation = variance.sqrt();
            if standard_deviation.is_finite() && standard_deviation > 0.0 {
                Some(standard_deviation)
            } else {
                None
            }
        } else {
            None
        }
    }
}

struct RollingVolumeMedian {
    bars: VecDeque<i64>,
    bar_sum: i128,
    prior_sums: VecDeque<i128>,
}

impl RollingVolumeMedian {
    fn new() -> Self {
        Self {
            bars: VecDeque::with_capacity(VOLUME_WINDOW + 1),
            bar_sum: 0,
            prior_sums: VecDeque::with_capacity(PRIOR_VOLUME_WINDOWS + 1),
        }
    }

    // Returns the current 60-minute sum and twice the median of the prior 24 hours.
    fn push(&mut self, volume: Qty) -> Option<(i128, i128)> {
        let volume = volume.raw().max(0);
        self.bars.push_back(volume);
        self.bar_sum += i128::from(volume);
        if self.bars.len() > VOLUME_WINDOW {
            self.bar_sum -= i128::from(self.bars.pop_front().expect("non-empty"));
        }
        if self.bars.len() < VOLUME_WINDOW {
            return None;
        }

        let current_sum = self.bar_sum;
        let median_twice = if self.prior_sums.len() == PRIOR_VOLUME_WINDOWS {
            let mut sorted: Vec<i128> = self.prior_sums.iter().copied().collect();
            sorted.sort_unstable();
            let middle = sorted.len() / 2;
            if sorted.len() % 2 == 0 {
                Some(sorted[middle - 1] + sorted[middle])
            } else {
                Some(sorted[middle] * 2)
            }
        } else {
            None
        };

        self.prior_sums.push_back(current_sum);
        if self.prior_sums.len() > PRIOR_VOLUME_WINDOWS {
            self.prior_sums.pop_front();
        }

        median_twice.map(|median| (current_sum, median))
    }
}

struct QuietFlowVwapReversion {
    vwap: RollingVwap,
    residual_std: RollingResidualStd,
    volumes: RollingVolumeMedian,
    entry_ts: Option<i64>,
    reentry_locked: bool,
}

impl QuietFlowVwapReversion {
    fn new() -> Self {
        Self {
            vwap: RollingVwap::new(),
            residual_std: RollingResidualStd::new(),
            volumes: RollingVolumeMedian::new(),
            entry_ts: None,
            reentry_locked: false,
        }
    }
}

impl Strategy for QuietFlowVwapReversion {
    fn name(&self) -> String {
        "quiet-flow-vwap-reversion".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let vwap_raw = self.vwap.push(bar);
        let volume_comparison = self.volumes.push(bar.volume);
        let Some(vwap_raw) = vwap_raw else {
            return;
        };

        let residual = bar.close.raw().saturating_sub(vwap_raw);
        let Some(standard_deviation) = self.residual_std.push(residual) else {
            return;
        };
        let z = residual as f64 / standard_deviation;

        // Market orders submitted after this close are handled by the sandbox's
        // execution timing and fill model.
        if ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            let time_expired = self.entry_ts.is_some_and(|entry| {
                bar.close_ts.as_nanos().saturating_sub(entry)
                    >= TimeDelta::from_hours(12).as_nanos()
            });
            if bar.close.raw() >= vwap_raw || z <= -3.0 || time_expired {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                self.reentry_locked = true;
            }
            return;
        }

        if !position.is_zero() {
            return;
        }

        if self.reentry_locked {
            if z > -0.5 {
                self.reentry_locked = false;
            } else {
                return;
            }
        }

        let Some((current_volume, median_twice)) = volume_comparison else {
            return;
        };
        // current <= 1.25 * median, with the median represented doubled.
        let quiet_volume = current_volume * 8 <= median_twice * 5;
        if z > -1.5 || !quiet_volume {
            return;
        }

        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        let volume_cap = Qty::from_raw(bar.volume.raw().max(0) / 10);
        let qty = if affordable < volume_cap {
            affordable
        } else {
            volume_cap
        };
        if qty.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        match fill.side {
            Side::Buy => {
                if self.entry_ts.is_none() {
                    self.entry_ts = Some(fill.ts.as_nanos());
                }
            }
            Side::Sell => {
                if ctx.position(INSTRUMENT).is_zero() {
                    self.entry_ts = None;
                }
            }
        }
    }
}

export_strategy!(QuietFlowVwapReversion::new());
