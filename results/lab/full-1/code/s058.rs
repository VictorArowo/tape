use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const RETURNS_PER_DAY: usize = 1_440;
const DAILY_BASELINE_LEN: usize = 30;
const SIZING_MARGIN_PPM: i64 = 5_000;
const EXIT_RATIO: f64 = 2.0;
const ENTRY_RATIO: f64 = 1.25;

#[derive(Clone, Copy)]
struct ReturnPoint {
    log_return: f64,
    downside_sq: f64,
}

struct DownsideSemivarianceGate {
    previous_close: Option<f64>,
    returns: VecDeque<ReturnPoint>,
    log_return_sum: f64,
    downside_sq_sum: f64,
    daily_readings: VecDeque<f64>,
    started: bool,
}

impl DownsideSemivarianceGate {
    fn new() -> Self {
        Self {
            previous_close: None,
            returns: VecDeque::with_capacity(RETURNS_PER_DAY + 1),
            log_return_sum: 0.0,
            downside_sq_sum: 0.0,
            daily_readings: VecDeque::with_capacity(DAILY_BASELINE_LEN + 1),
            started: false,
        }
    }

    fn push_close(&mut self, close: f64) {
        if let Some(previous) = self.previous_close {
            if previous > 0.0 && close > 0.0 {
                let log_return = (close / previous).ln();
                let downside_sq = if log_return < 0.0 {
                    log_return * log_return
                } else {
                    0.0
                };
                let point = ReturnPoint {
                    log_return,
                    downside_sq,
                };
                self.returns.push_back(point);
                self.log_return_sum += log_return;
                self.downside_sq_sum += downside_sq;

                if self.returns.len() > RETURNS_PER_DAY {
                    if let Some(oldest) = self.returns.pop_front() {
                        self.log_return_sum -= oldest.log_return;
                        self.downside_sq_sum -= oldest.downside_sq;
                    }
                }
            }
        }
        self.previous_close = Some(close);
    }

    fn has_full_day(&self) -> bool {
        self.returns.len() == RETURNS_PER_DAY
    }

    fn semivariance(&self) -> f64 {
        self.downside_sq_sum.max(0.0).sqrt()
    }

    fn push_daily_reading(&mut self, reading: f64) {
        self.daily_readings.push_back(reading);
        if self.daily_readings.len() > DAILY_BASELINE_LEN {
            self.daily_readings.pop_front();
        }
    }

    fn baseline_median(&self) -> Option<f64> {
        if self.daily_readings.len() < DAILY_BASELINE_LEN {
            return None;
        }
        let mut sorted: Vec<f64> = self.daily_readings.iter().copied().collect();
        sorted.sort_by(f64::total_cmp);
        let middle = sorted.len() / 2;
        Some((sorted[middle - 1] + sorted[middle]) * 0.5)
    }

    fn ratio(current: f64, baseline: f64) -> f64 {
        if baseline > 0.0 {
            current / baseline
        } else if current > 0.0 {
            f64::INFINITY
        } else {
            1.0
        }
    }

    fn submit_buy(&self, ctx: &mut Ctx, price: tape_guest::Price) {
        if ctx.open_orders() != 0 || !price.is_positive() {
            return;
        }
        let qty = ctx.affordable_qty(
            INSTRUMENT,
            price,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        if qty.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }
}

impl Strategy for DownsideSemivarianceGate {
    fn name(&self) -> String {
        "downside-semivariance-gate".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        self.push_close(bar.close.to_f64());

        // Establish the initial fully invested position on the first available bar.
        if !self.started {
            self.started = true;
            if ctx.position(INSTRUMENT).is_zero() {
                self.submit_buy(ctx, bar.close);
            }
        }

        if !self.has_full_day() {
            return;
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let minute_of_day = (bar.close_ts.as_nanos() / minute_ns) % 1_440;

        // Save a completed daily reading before evaluating a midnight hourly signal.
        if minute_of_day == 0 {
            self.push_daily_reading(self.semivariance());
        }

        // Signals are evaluated only at completed UTC hours.
        if minute_of_day % 60 != 0 {
            return;
        }

        let Some(baseline) = self.baseline_median() else {
            return;
        };
        let ratio = Self::ratio(self.semivariance(), baseline);
        let position = ctx.position(INSTRUMENT);

        if ctx.open_orders() != 0 {
            return;
        }

        if position.is_positive() {
            if ratio >= EXIT_RATIO && self.log_return_sum < 0.0 {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
        } else if ratio <= ENTRY_RATIO && self.log_return_sum > 0.0 {
            self.submit_buy(ctx, bar.close);
        }
    }
}

export_strategy!(DownsideSemivarianceGate::new());