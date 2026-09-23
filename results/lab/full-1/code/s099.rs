use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta, TimeInForce, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const RETURN_WINDOW: usize = 30 * 24 * 60;
const SIZING_MARGIN_PPM: i64 = 5_000;
const QTY_SCALE: f64 = 100_000_000.0;
const MIN_REBALANCE: f64 = 0.10;

struct DailyVolBudget {
    returns: VecDeque<f64>,
    return_sum: f64,
    return_sq_sum: f64,
    previous_close: Option<Price>,
    previous_close_ns: Option<i64>,
}

impl DailyVolBudget {
    fn new() -> Self {
        Self {
            returns: VecDeque::with_capacity(RETURN_WINDOW),
            return_sum: 0.0,
            return_sq_sum: 0.0,
            previous_close: None,
            previous_close_ns: None,
        }
    }

    fn clear_returns(&mut self) {
        self.returns.clear();
        self.return_sum = 0.0;
        self.return_sq_sum = 0.0;
    }

    fn push_return(&mut self, value: f64) {
        self.returns.push_back(value);
        self.return_sum += value;
        self.return_sq_sum += value * value;

        if self.returns.len() > RETURN_WINDOW {
            if let Some(old) = self.returns.pop_front() {
                self.return_sum -= old;
                self.return_sq_sum -= old * old;
            }
        }
    }

    fn update_returns(&mut self, bar: &Bar, minute_ns: i64) {
        let close_ns = bar.close_ts.as_nanos();
        if let (Some(previous_close), Some(previous_ns)) =
            (self.previous_close, self.previous_close_ns)
        {
            let consecutive = close_ns.checked_sub(previous_ns) == Some(minute_ns);
            let previous_price = previous_close.to_f64();
            let current_price = bar.close.to_f64();

            if consecutive && previous_price > 0.0 && current_price > 0.0 {
                self.push_return((current_price / previous_price).ln());
            } else {
                // A gap means the rolling window no longer represents consecutive minutes.
                self.clear_returns();
            }
        }

        self.previous_close = Some(bar.close);
        self.previous_close_ns = Some(close_ns);
    }

    fn target_weight(&self) -> Option<f64> {
        if self.returns.len() != RETURN_WINDOW {
            return None;
        }

        // Use the sample standard deviation of the observed one-minute returns.
        let count = self.returns.len() as f64;
        let numerator = self.return_sq_sum - self.return_sum * self.return_sum / count;
        let variance = (numerator / (count - 1.0)).max(0.0);
        let sigma = variance.sqrt() * 525_600.0_f64.sqrt();

        if !sigma.is_finite() || sigma == 0.0 {
            return Some(1.0);
        }

        Some((0.80 / sigma).clamp(0.0, 1.0))
    }

    fn submit_rebalance(&self, ctx: &mut Ctx, bar: &Bar, target_weight: f64) {
        if ctx.open_orders() != 0 {
            return;
        }

        let price = bar.close.to_f64();
        let nav = ctx.equity().to_f64();
        if !price.is_finite() || price <= 0.0 || !nav.is_finite() || nav <= 0.0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        let current_weight = position.to_f64() * price / nav;
        if !current_weight.is_finite() || (target_weight - current_weight).abs() < MIN_REBALANCE {
            return;
        }

        let target_qty = target_weight * nav / price;
        let delta_qty = target_qty - position.to_f64();
        if !delta_qty.is_finite() {
            return;
        }

        if delta_qty > 0.0 {
            let requested_raw = (delta_qty * QTY_SCALE).floor();
            if !requested_raw.is_finite() || requested_raw < 1.0 || requested_raw > i64::MAX as f64 {
                return;
            }
            let requested = Qty::from_raw(requested_raw as i64);
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            let qty = Qty::from_raw(requested.raw().min(affordable.raw()));
            if qty.is_positive() {
                ctx.submit(
                    OrderRequest::market(INSTRUMENT, Side::Buy, qty)
                        .with_time_in_force(TimeInForce::Ioc),
                );
            }
        } else if delta_qty < 0.0 && position.is_positive() {
            let requested_raw = (-delta_qty * QTY_SCALE).floor();
            if !requested_raw.is_finite() || requested_raw < 1.0 || requested_raw > i64::MAX as f64 {
                return;
            }
            let qty = Qty::from_raw((requested_raw as i64).min(position.raw()));
            if qty.is_positive() {
                ctx.submit(
                    OrderRequest::market(INSTRUMENT, Side::Sell, qty)
                        .with_time_in_force(TimeInForce::Ioc),
                );
            }
        }
    }
}

impl Strategy for DailyVolBudget {
    fn name(&self) -> String {
        "daily-volatility-budget".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        self.update_returns(bar, minute_ns);

        let open_day = bar.open_ts.as_nanos().div_euclid(day_ns);
        let close_ns = bar.close_ts.as_nanos();
        let close_day = close_ns.div_euclid(day_ns);
        let minute_of_day = close_ns.rem_euclid(day_ns) / minute_ns;
        let day_has_closed = minute_of_day == 1_439 || open_day != close_day;
        if !day_has_closed {
            return;
        }

        // Do not begin rebalancing until a full 30-day return window is available.
        if let Some(target_weight) = self.target_weight() {
            self.submit_rebalance(ctx, bar, target_weight);
        }
    }

    fn on_finish(&mut self, ctx: &mut Ctx) {
        if ctx.open_orders() != 0 {
            return;
        }
        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            ctx.submit(
                OrderRequest::market(INSTRUMENT, Side::Sell, position)
                    .with_time_in_force(TimeInForce::Ioc),
            );
        }
    }
}

export_strategy!(DailyVolBudget::new());
