use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, TimeInForce,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const RETURN_WINDOW: usize = 10_080;
const ANNUAL_MINUTES: f64 = 525_600.0;
const TARGET_VOL_NUMERATOR: f64 = 0.50;
const REBALANCE_THRESHOLD: f64 = 0.05;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct VolatilityScaledSpotExposure {
    returns_squared: VecDeque<f64>,
    sum_squared: f64,
    previous_close: Option<f64>,
    last_day: Option<i64>,
}

fn qty_from_units(units: f64) -> Qty {
    if !units.is_finite() || units <= 0.0 {
        return Qty::from_raw(0);
    }
    Qty::from_raw((units * 1e8).floor() as i64)
}

fn min_qty(a: Qty, b: Qty) -> Qty {
    Qty::from_raw(a.raw().min(b.raw()))
}

impl Strategy for VolatilityScaledSpotExposure {
    fn name(&self) -> String {
        "volatility-scaled-spot-exposure".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT || !bar.close.is_positive() {
            return;
        }

        let close = bar.close.to_f64();
        if let Some(previous) = self.previous_close {
            if previous > 0.0 {
                let ret = close / previous - 1.0;
                let squared = ret * ret;
                self.returns_squared.push_back(squared);
                self.sum_squared += squared;
                if self.returns_squared.len() > RETURN_WINDOW {
                    self.sum_squared -= self.returns_squared.pop_front().unwrap_or(0.0);
                }
            }
        }
        self.previous_close = Some(close);

        let day_ns = TimeDelta::from_days(1).as_nanos();
        if day_ns <= 0 {
            return;
        }
        let day = bar.close_ts.as_nanos() / day_ns;
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let minute_of_day = if minute_ns > 0 {
            (bar.close_ts.as_nanos() / minute_ns) % 1_440
        } else {
            -1
        };
        let first_bar_of_day = match self.last_day {
            Some(previous_day) => day != previous_day,
            None => minute_of_day == 0,
        };
        self.last_day = Some(day);

        if !first_bar_of_day || self.returns_squared.len() < RETURN_WINDOW {
            return;
        }

        let mean_squared = self.sum_squared / RETURN_WINDOW as f64;
        let sigma = (ANNUAL_MINUTES * mean_squared).sqrt();
        if !sigma.is_finite() {
            return;
        }
        let target_weight = if sigma > 0.0 {
            (TARGET_VOL_NUMERATOR / sigma).clamp(0.10, 1.00)
        } else {
            1.00
        };

        if ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        let held_position = if position.is_positive() {
            position
        } else {
            Qty::from_raw(0)
        };
        let held_value = bar.close * held_position;
        let equity = ctx.cash() + held_value;
        let equity_f = equity.to_f64();
        if equity_f <= 0.0 {
            return;
        }

        let current_weight = held_value.to_f64() / equity_f;
        let weight_difference = target_weight - current_weight;
        if weight_difference.abs() < REBALANCE_THRESHOLD {
            return;
        }

        let desired_qty = qty_from_units(weight_difference.abs() * equity_f / close);
        if !desired_qty.is_positive() {
            return;
        }

        // The SDK has no participation-rate order type. Bound the submitted IOC
        // quantity using the latest completed bar's volume as the available proxy.
        let volume_cap = bar.volume.mul_ratio(1, 10);
        let qty = if weight_difference > 0.0 {
            // Include the specified 2 bps slippage in the affordability estimate;
            // the execution engine must apply actual fees and fill slippage.
            let estimated_buy_price = bar.close.mul_ratio(10_002, 10_000);
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                estimated_buy_price,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            min_qty(min_qty(desired_qty, affordable), volume_cap)
        } else {
            min_qty(min_qty(desired_qty, held_position), volume_cap)
        };

        if !qty.is_positive() {
            return;
        }

        let side = if weight_difference > 0.0 {
            Side::Buy
        } else {
            Side::Sell
        };
        ctx.submit(
            OrderRequest::market(INSTRUMENT, side, qty)
                .with_time_in_force(TimeInForce::Ioc),
        );
    }
}

export_strategy!(VolatilityScaledSpotExposure {
    returns_squared: VecDeque::with_capacity(RETURN_WINDOW + 1),
    sum_squared: 0.0,
    previous_close: None,
    last_day: None,
});
