use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, Money, OrderRequest, Price, Qty, Side, Strategy, TimeDelta,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const RV_WINDOW: usize = 1_440;
const DAILY_BASELINE: usize = 14;
const MIN_RETURNS_PER_DAY: usize = 1_439;
const SIZING_MARGIN_PPM: i64 = 5_000;
const SCALE: i64 = 100_000_000;
const EXIT_Q: f64 = 1.50;
const ENTRY_Q: f64 = 1.15;

struct RealizedVarianceGate {
    returns_sq: VecDeque<f64>,
    daily_rvs: VecDeque<f64>,
    current_day: Option<i64>,
    day_sum_sq: f64,
    day_return_count: usize,
    day_valid: bool,
    previous_close: Option<Price>,
    previous_ts: Option<i64>,
}

impl RealizedVarianceGate {
    fn new() -> Self {
        Self {
            returns_sq: VecDeque::with_capacity(RV_WINDOW + 1),
            daily_rvs: VecDeque::with_capacity(DAILY_BASELINE + 1),
            current_day: None,
            day_sum_sq: 0.0,
            day_return_count: 0,
            day_valid: true,
            previous_close: None,
            previous_ts: None,
        }
    }

    fn median_daily_rv(&self) -> Option<f64> {
        if self.daily_rvs.len() != DAILY_BASELINE {
            return None;
        }
        let mut values: Vec<f64> = self.daily_rvs.iter().copied().collect();
        values.sort_by(|a, b| a.total_cmp(b));
        Some((values[DAILY_BASELINE / 2 - 1] + values[DAILY_BASELINE / 2]) * 0.5)
    }

    fn is_checkpoint(ts: i64, minute_ns: i64) -> bool {
        if ts.rem_euclid(minute_ns) != 0 {
            return false;
        }
        let minute_of_day = ts.div_euclid(minute_ns).rem_euclid(1_440);
        matches!(minute_of_day, 0 | 360 | 720 | 1_080)
    }

    fn finish_current_day(&mut self) {
        if self.day_valid && self.day_return_count >= MIN_RETURNS_PER_DAY {
            self.daily_rvs.push_back(self.day_sum_sq.sqrt());
            if self.daily_rvs.len() > DAILY_BASELINE {
                self.daily_rvs.pop_front();
            }
        } else {
            // An incomplete or interrupted day cannot be part of the immediately
            // preceding 14-day baseline.
            self.daily_rvs.clear();
        }
    }

    fn submit_entry(&self, ctx: &mut Ctx, price: Price) {
        if !price.is_positive() {
            return;
        }
        let equity_raw = ctx.equity().raw();
        let budget_raw = ctx.cash().raw().min(equity_raw);
        if budget_raw <= 0 || equity_raw <= 0 {
            return;
        }

        // affordable_qty accounts for fees. The notional cap also ensures the
        // requested BTC exposure does not exceed account equity at the reference price.
        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            price,
            Money::from_raw(budget_raw),
            SIZING_MARGIN_PPM,
        );
        let max_notional_qty = (i128::from(equity_raw) * i128::from(SCALE))
            / i128::from(price.raw());
        let max_notional_qty = max_notional_qty.min(i128::from(i64::MAX)) as i64;
        let qty = Qty::from_raw(affordable.raw().min(max_notional_qty));
        if qty.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }
}

impl Strategy for RealizedVarianceGate {
    fn name(&self) -> String {
        "realized-variance-gate".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let now_ns = bar.close_ts.as_nanos();
        let day_id = now_ns.div_euclid(day_ns);

        // Ignore out-of-order bars rather than corrupting the rolling windows.
        if self.current_day.is_some_and(|day| day_id < day) {
            return;
        }

        // A gap breaks both the rolling minute window and the completeness of
        // the current daily observation. Do this before any checkpoint decision.
        let contiguous = self
            .previous_ts
            .map(|previous| now_ns.checked_sub(previous) == Some(minute_ns));
        if contiguous == Some(false) {
            self.returns_sq.clear();
            if self.current_day.is_some() {
                self.day_valid = false;
            }
        }

        match self.current_day {
            None => {
                self.current_day = Some(day_id);
                self.day_sum_sq = 0.0;
                self.day_return_count = 0;
                self.day_valid = true;
            }
            Some(previous_day) if day_id > previous_day => {
                if day_id == previous_day.saturating_add(1) {
                    self.finish_current_day();
                } else {
                    self.daily_rvs.clear();
                }
                self.current_day = Some(day_id);
                self.day_sum_sq = 0.0;
                self.day_return_count = 0;
                self.day_valid = true;
            }
            _ => {}
        }

        // The current bar is deliberately not added until after this decision:
        // checkpoint calculations use only returns from already-closed bars.
        if Self::is_checkpoint(now_ns, minute_ns)
            && contiguous == Some(true)
            && self.returns_sq.len() == RV_WINDOW
        {
            if let Some(median_rv) = self.median_daily_rv() {
                let rv24 = self.returns_sq.iter().sum::<f64>().sqrt();
                let q = if median_rv > 0.0 {
                    rv24 / median_rv
                } else if rv24 > 0.0 {
                    f64::INFINITY
                } else {
                    0.0
                };

                if ctx.open_orders() == 0 {
                    let position = ctx.position(INSTRUMENT);
                    if position.is_positive() && q >= EXIT_Q {
                        ctx.submit(OrderRequest::market(
                            INSTRUMENT,
                            Side::Sell,
                            position,
                        ));
                    } else if position.is_zero() && q <= ENTRY_Q {
                        self.submit_entry(ctx, bar.close);
                    }
                }
            }
        }

        if !bar.close.is_positive() {
            self.returns_sq.clear();
            self.day_valid = false;
            self.previous_close = None;
            self.previous_ts = Some(now_ns);
            return;
        }

        if contiguous == Some(true) {
            if let Some(previous_close) = self.previous_close {
                if previous_close.is_positive() {
                    let log_return = bar.close.to_f64().ln() - previous_close.to_f64().ln();
                    let squared_return = log_return * log_return;
                    if squared_return.is_finite() {
                        self.returns_sq.push_back(squared_return);
                        if self.returns_sq.len() > RV_WINDOW {
                            self.returns_sq.pop_front();
                        }
                        self.day_sum_sq += squared_return;
                        self.day_return_count += 1;
                    } else {
                        self.returns_sq.clear();
                        self.day_valid = false;
                    }
                } else {
                    self.returns_sq.clear();
                    self.day_valid = false;
                }
            } else {
                self.returns_sq.clear();
                self.day_valid = false;
            }
        } else if contiguous.is_none() {
            // The first observed bar has no prior close from which to form a return.
        } else {
            self.returns_sq.clear();
        }

        self.previous_close = Some(bar.close);
        self.previous_ts = Some(now_ns);
    }
}

export_strategy!(RealizedVarianceGate::new());