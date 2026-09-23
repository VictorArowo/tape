use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, TimeInForce,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const RETURN_WINDOW: usize = 10_080;
const ANNUAL_MINUTES: f64 = 525_600.0;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct CappedVolatilityBudget {
    previous_close: Option<i64>,
    returns: VecDeque<f64>,
}

impl CappedVolatilityBudget {
    fn push_return(&mut self, close_raw: i64) {
        if let Some(previous) = self.previous_close {
            if previous > 0 && close_raw > 0 {
                self.returns
                    .push_back(close_raw as f64 / previous as f64 - 1.0);
                if self.returns.len() > RETURN_WINDOW {
                    self.returns.pop_front();
                }
            }
        }
        self.previous_close = Some(close_raw);
    }

    fn annualized_volatility(&self) -> Option<f64> {
        if self.returns.len() != RETURN_WINDOW {
            return None;
        }

        let n = self.returns.len() as f64;
        let mean = self.returns.iter().sum::<f64>() / n;
        let squared_deviations = self
            .returns
            .iter()
            .map(|r| {
                let deviation = *r - mean;
                deviation * deviation
            })
            .sum::<f64>();
        let variance = (squared_deviations / (n - 1.0)).max(0.0);
        Some(variance.sqrt() * ANNUAL_MINUTES.sqrt())
    }

    fn is_utc_midnight(bar: &Bar) -> bool {
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        bar.close_ts
            .as_nanos()
            .div_euclid(minute_ns)
            .rem_euclid(1_440)
            == 0
    }

    fn qty_from_units(units: f64) -> Qty {
        if !units.is_finite() || units <= 0.0 {
            return Qty::from_raw(0);
        }
        Qty::from_raw((units * 1e8).floor() as i64)
    }
}

impl Strategy for CappedVolatilityBudget {
    fn name(&self) -> String {
        "capped-volatility-budget".to_owned()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT || !bar.close.is_positive() {
            return;
        }

        // The return ending at this midnight close is now observable and belongs
        // to the completed prior UTC day.
        self.push_return(bar.close.raw());

        if !Self::is_utc_midnight(bar) {
            return;
        }
        let Some(volatility) = self.annualized_volatility() else {
            return;
        };
        if ctx.open_orders() != 0 {
            return;
        }

        let equity = ctx.equity().to_f64();
        let price = bar.close.to_f64();
        if !equity.is_finite() || equity <= 0.0 || !price.is_finite() || price <= 0.0 {
            return;
        }

        let target_exposure = if volatility > 0.0 {
            (0.60 / volatility).clamp(0.25, 1.00)
        } else {
            1.00
        };
        let position = ctx.position(INSTRUMENT);
        let current_exposure = position.to_f64() * price / equity;
        if !current_exposure.is_finite()
            || (current_exposure - target_exposure).abs() <= 0.10
        {
            return;
        }

        let target_qty = Self::qty_from_units(target_exposure * equity / price);
        let volume_cap = bar.volume.mul_ratio(1, 10);
        if !volume_cap.is_positive() {
            return;
        }

        if current_exposure < target_exposure {
            let buy_units = target_qty.to_f64() - position.to_f64();
            let mut qty = Self::qty_from_units(buy_units);
            if !qty.is_positive() {
                return;
            }
            if qty > volume_cap {
                qty = volume_cap;
            }
            // Use the SDK's fee-aware affordability check for every buy.
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty > affordable {
                qty = affordable;
            }
            if qty.is_positive() {
                ctx.submit(
                    OrderRequest::market(INSTRUMENT, Side::Buy, qty)
                        .with_time_in_force(TimeInForce::Ioc),
                );
            }
        } else if position.is_positive() {
            let sell_units = position.to_f64() - target_qty.to_f64();
            let mut qty = Self::qty_from_units(sell_units);
            if qty > position {
                qty = position;
            }
            if qty > volume_cap {
                qty = volume_cap;
            }
            if qty.is_positive() {
                ctx.submit(
                    OrderRequest::market(INSTRUMENT, Side::Sell, qty)
                        .with_time_in_force(TimeInForce::Ioc),
                );
            }
        }
    }
}

export_strategy!(CappedVolatilityBudget {
    previous_close: None,
    returns: VecDeque::with_capacity(RETURN_WINDOW + 1),
});
