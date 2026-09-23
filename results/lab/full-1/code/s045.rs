use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, Money, OrderRequest, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIX_HOURS: usize = 360;
const THIRTY_MINUTES: usize = 30;
const HOURLY_SAMPLES: usize = 14 * 24;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct QuietToActiveRelease {
    closes: VecDeque<i64>,
    returns: VecDeque<f64>,
    hourly_volatility: VecDeque<(i64, f64)>,
    compression_active: bool,
    entered_this_episode: bool,
    position_started_ns: Option<i64>,
    cooldown_until_ns: Option<i64>,
}

impl QuietToActiveRelease {
    fn new() -> Self {
        Self {
            closes: VecDeque::with_capacity(THIRTY_MINUTES + 1),
            returns: VecDeque::with_capacity(SIX_HOURS),
            hourly_volatility: VecDeque::with_capacity(HOURLY_SAMPLES),
            compression_active: false,
            entered_this_episode: false,
            position_started_ns: None,
            cooldown_until_ns: None,
        }
    }

    fn six_hour_volatility(&self) -> Option<f64> {
        if self.returns.len() < SIX_HOURS {
            return None;
        }
        let sum_squares: f64 = self
            .returns
            .iter()
            .rev()
            .take(SIX_HOURS)
            .map(|value| value * value)
            .sum();
        Some(sum_squares.sqrt())
    }

    fn thirty_minute_signal(&self) -> Option<(f64, f64)> {
        if self.closes.len() < THIRTY_MINUTES + 1 || self.returns.len() < THIRTY_MINUTES {
            return None;
        }

        let old_close = *self.closes.front()?;
        let latest_close = *self.closes.back()?;
        if old_close <= 0 || latest_close <= 0 {
            return None;
        }

        let return_30m = (latest_close as f64 / old_close as f64).ln();
        let realized_volatility = self
            .returns
            .iter()
            .rev()
            .take(THIRTY_MINUTES)
            .map(|value| value * value)
            .sum::<f64>()
            .sqrt();
        Some((return_30m, realized_volatility))
    }

    fn update_compression(&mut self, sample_ts_ns: i64, current_volatility: f64) {
        let lookback_ns = TimeDelta::from_days(14).as_nanos();
        let cutoff = sample_ts_ns.saturating_sub(lookback_ns);
        while self
            .hourly_volatility
            .front()
            .is_some_and(|(ts, _)| *ts < cutoff)
        {
            self.hourly_volatility.pop_front();
        }

        let is_compressed = if self.hourly_volatility.len() >= HOURLY_SAMPLES {
            let mut sorted: Vec<f64> = self
                .hourly_volatility
                .iter()
                .map(|(_, volatility)| *volatility)
                .collect();
            sorted.sort_by(f64::total_cmp);
            let percentile_index = (sorted.len() * 20).div_ceil(100).saturating_sub(1);
            current_volatility <= sorted[percentile_index]
        } else {
            false
        };

        self.compression_active = is_compressed;
        if !is_compressed {
            self.entered_this_episode = false;
        }

        self.hourly_volatility
            .push_back((sample_ts_ns, current_volatility));
        while self.hourly_volatility.len() > HOURLY_SAMPLES {
            self.hourly_volatility.pop_front();
        }
    }
}

impl Strategy for QuietToActiveRelease {
    fn name(&self) -> String {
        "quiet-to-active-release".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        if let Some(previous_close) = self.closes.back().copied() {
            if previous_close > 0 && bar.close.raw() > 0 {
                let minute_return = (bar.close.raw() as f64 / previous_close as f64).ln();
                self.returns.push_back(minute_return);
                if self.returns.len() > SIX_HOURS {
                    self.returns.pop_front();
                }
            }
        }
        self.closes.push_back(bar.close.raw());
        if self.closes.len() > THIRTY_MINUTES + 1 {
            self.closes.pop_front();
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let minute_index = bar.close_ts.as_nanos() / minute_ns;
        if minute_index.rem_euclid(60) == 0 {
            if let Some(volatility) = self.six_hour_volatility() {
                self.update_compression(bar.close_ts.as_nanos(), volatility);
            }
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            if let Some(started_ns) = self.position_started_ns {
                let hold_ns = TimeDelta::from_hours(6).as_nanos();
                if bar.close_ts.as_nanos() >= started_ns.saturating_add(hold_ns)
                    && ctx.open_orders() == 0
                {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                }
            }
            return;
        }

        let now_ns = bar.close_ts.as_nanos();
        if self
            .cooldown_until_ns
            .is_some_and(|until_ns| now_ns < until_ns)
        {
            return;
        }
        if !self.compression_active || self.entered_this_episode || ctx.open_orders() != 0 {
            return;
        }

        let Some((return_30m, volatility_30m)) = self.thirty_minute_signal() else {
            return;
        };
        if return_30m <= 1.5 * volatility_30m {
            return;
        }

        let budget: Money = ctx.cash().mul_ratio(95, 100);
        let quantity = ctx.affordable_qty(INSTRUMENT, bar.close, budget, SIZING_MARGIN_PPM);
        if quantity.is_positive() {
            self.entered_this_episode = true;
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, quantity));
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.side == Side::Buy {
            if self.position_started_ns.is_none() {
                self.position_started_ns = Some(fill.ts.as_nanos());
            }
        } else if fill.side == Side::Sell && ctx.position(INSTRUMENT).is_zero() {
            self.position_started_ns = None;
            self.cooldown_until_ns = Some(
                fill.ts
                    .as_nanos()
                    .saturating_add(TimeDelta::from_hours(6).as_nanos()),
            );
        }
    }
}

export_strategy!(QuietToActiveRelease::new());