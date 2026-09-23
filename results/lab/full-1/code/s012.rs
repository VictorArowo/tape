use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const RETURN_WINDOW: usize = 48;
const HISTORY_WINDOW: usize = 720;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct HourAggregator {
    bucket: Option<i64>,
    last_minute: Option<i64>,
    count: usize,
    valid: bool,
}

impl HourAggregator {
    fn new() -> Self {
        Self {
            bucket: None,
            last_minute: None,
            count: 0,
            valid: false,
        }
    }

    // Bars are timestamped at their close, so assign each close to the
    // one-minute interval ending there.
    fn push(&mut self, bar: &Bar) -> Option<(i64, Price)> {
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let minute = bar.close_ts.as_nanos().div_euclid(minute_ns) - 1;
        let hour = minute.div_euclid(60);
        let slot = minute.rem_euclid(60);

        if self.bucket != Some(hour) {
            self.bucket = Some(hour);
            self.last_minute = Some(minute);
            self.count = 1;
            self.valid = slot == 0;
        } else {
            let contiguous = self.last_minute == Some(minute - 1);
            if contiguous && self.valid {
                self.count += 1;
            } else {
                self.valid = false;
            }
            self.last_minute = Some(minute);
        }

        if slot == 59 && self.valid && self.count == 60 {
            Some((hour, bar.close))
        } else {
            None
        }
    }
}

struct VolatilityRegimeFilter {
    hours: HourAggregator,
    previous_hour: Option<(i64, f64)>,
    returns: VecDeque<f64>,
    volatility_history: VecDeque<f64>,
}

impl VolatilityRegimeFilter {
    fn new() -> Self {
        Self {
            hours: HourAggregator::new(),
            previous_hour: None,
            returns: VecDeque::with_capacity(RETURN_WINDOW + 1),
            volatility_history: VecDeque::with_capacity(HISTORY_WINDOW + 1),
        }
    }

    fn on_hour_close(&mut self, ctx: &mut Ctx, hour: i64, close: Price) {
        let close_value = close.to_f64();
        if !close_value.is_finite() || close_value <= 0.0 {
            self.previous_hour = None;
            self.returns.clear();
            return;
        }

        let Some((previous_hour, previous_close)) = self.previous_hour else {
            self.previous_hour = Some((hour, close_value));
            return;
        };

        if hour != previous_hour + 1 {
            // Do not calculate a one-hour return across missing hourly bars.
            self.returns.clear();
            self.previous_hour = Some((hour, close_value));
            return;
        }

        let log_return = (close_value / previous_close).ln();
        self.previous_hour = Some((hour, close_value));
        if !log_return.is_finite() {
            self.returns.clear();
            return;
        }

        self.returns.push_back(log_return);
        if self.returns.len() > RETURN_WINDOW {
            self.returns.pop_front();
        }
        if self.returns.len() != RETURN_WINDOW {
            return;
        }

        let sum_squares: f64 = self.returns.iter().map(|r| r * r).sum();
        let volatility = sum_squares.sqrt();
        if !volatility.is_finite() {
            return;
        }

        // Thresholds use only the preceding readings; add this reading after
        // making the decision.
        if self.volatility_history.len() == HISTORY_WINDOW && ctx.open_orders() == 0 {
            let p45 = Self::nearest_rank_percentile(&self.volatility_history, 45);
            let p55 = Self::nearest_rank_percentile(&self.volatility_history, 55);
            let position = ctx.position(INSTRUMENT);

            if position.is_zero() && volatility <= p45 {
                let qty = ctx.affordable_qty(
                    INSTRUMENT,
                    close,
                    ctx.cash(),
                    SIZING_MARGIN_PPM,
                );
                if qty.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                }
            } else if position.is_positive() && volatility >= p55 {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
        }

        self.volatility_history.push_back(volatility);
        if self.volatility_history.len() > HISTORY_WINDOW {
            self.volatility_history.pop_front();
        }
    }

    fn nearest_rank_percentile(history: &VecDeque<f64>, percentile: usize) -> f64 {
        let mut sorted: Vec<f64> = history.iter().copied().collect();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let rank = (sorted.len() * percentile + 99) / 100;
        sorted[rank.saturating_sub(1)]
    }
}

impl Strategy for VolatilityRegimeFilter {
    fn name(&self) -> String {
        "Volatility Regime Filter".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }
        if let Some((hour, close)) = self.hours.push(bar) {
            self.on_hour_close(ctx, hour, close);
        }
    }
}

export_strategy!(VolatilityRegimeFilter::new());
