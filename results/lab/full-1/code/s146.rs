use std::collections::VecDeque;

use tape_guest::{
    export_strategy, Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const RETURN_COUNT: usize = 168;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct WeeklyVolatilityScaling {
    hourly_returns: VecDeque<f64>,
    last_hour_close: Option<f64>,
    initialized: bool,
}

impl WeeklyVolatilityScaling {
    fn target_weight(&self) -> Option<f64> {
        if self.hourly_returns.len() < RETURN_COUNT {
            return None;
        }

        let count = self.hourly_returns.len() as f64;
        let mean = self.hourly_returns.iter().sum::<f64>() / count;
        let squared_deviations = self
            .hourly_returns
            .iter()
            .map(|r| {
                let deviation = *r - mean;
                deviation * deviation
            })
            .sum::<f64>();
        // Use the sample standard deviation of the 168 hourly returns.
        let variance = squared_deviations / (count - 1.0);
        let annualized_volatility = variance.max(0.0).sqrt() * 8760.0_f64.sqrt();

        if !annualized_volatility.is_finite() {
            return None;
        }
        if annualized_volatility <= 0.0 {
            return Some(1.0);
        }

        Some((0.70 / annualized_volatility).clamp(0.25, 1.00))
    }
}

fn rebalance_to_weight(ctx: &mut Ctx, price: Price, weight: f64) -> bool {
    if ctx.open_orders() != 0 || !price.is_positive() {
        return false;
    }

    let equity = ctx.equity();
    if !equity.is_positive() {
        return false;
    }

    let desired_qty_raw = (equity.to_f64() * weight / price.to_f64()) * 100_000_000.0;
    if !desired_qty_raw.is_finite() || desired_qty_raw < 0.0 || desired_qty_raw > i64::MAX as f64 {
        return false;
    }
    let desired_qty = Qty::from_raw(desired_qty_raw.floor() as i64);
    let position = ctx.position(INSTRUMENT);

    if desired_qty.raw() > position.raw() {
        let needed_raw = desired_qty.raw().saturating_sub(position.raw());
        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            price,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        let qty_raw = needed_raw.min(affordable.raw());
        if qty_raw > 0 {
            ctx.submit(OrderRequest::market(
                INSTRUMENT,
                Side::Buy,
                Qty::from_raw(qty_raw),
            ));
            return true;
        }
        return false;
    }

    if position.raw() > desired_qty.raw() {
        let qty = Qty::from_raw(position.raw() - desired_qty.raw());
        if qty.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
        }
    }

    true
}

impl Strategy for WeeklyVolatilityScaling {
    fn name(&self) -> String {
        "weekly-volatility-scaling".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        // Establish the initial full BTC allocation, subject to fee-safe sizing.
        if !self.initialized && rebalance_to_weight(ctx, bar.close, 1.0) {
            self.initialized = true;
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let timestamp_ns = bar.close_ts.as_nanos();
        let minute_of_day = (timestamp_ns / minute_ns).rem_euclid(1440);
        let day_of_week = (timestamp_ns / day_ns).rem_euclid(7); // 0 = Thursday

        // A bar closing at minute zero completes an hourly observation.
        if minute_of_day % 60 == 0 {
            let close = bar.close.to_f64();
            if close.is_finite() && close > 0.0 {
                if let Some(previous) = self.last_hour_close {
                    let log_return = (close / previous).ln();
                    if log_return.is_finite() {
                        self.hourly_returns.push_back(log_return);
                        if self.hourly_returns.len() > RETURN_COUNT {
                            self.hourly_returns.pop_front();
                        }
                    }
                }
                self.last_hour_close = Some(close);
            } else {
                self.last_hour_close = None;
            }
        }

        // Monday is day 4 when Thursday is day 0. The market order is submitted
        // on this bar so it can execute at the following bar's open.
        if minute_of_day == 0 && day_of_week == 4 {
            if let Some(target_weight) = self.target_weight() {
                let equity = ctx.equity();
                if equity.is_positive() {
                    let current_weight = (bar.close * ctx.position(INSTRUMENT)).to_f64()
                        / equity.to_f64();
                    if current_weight.is_finite()
                        && (target_weight - current_weight).abs() >= 0.05
                        && ctx.open_orders() == 0
                    {
                        rebalance_to_weight(ctx, bar.close, target_weight);
                    }
                }
            }
        }
    }
}

export_strategy!(WeeklyVolatilityScaling {
    hourly_returns: VecDeque::with_capacity(RETURN_COUNT + 1),
    last_hour_close: None,
    initialized: false,
});
