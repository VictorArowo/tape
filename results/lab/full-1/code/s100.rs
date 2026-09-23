use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const RETURNS_WINDOW: usize = 30;
const SIZING_MARGIN_PPM: i64 = 1_000_000; // Fully cash-backed; no borrowing.
const RAW_SCALE: f64 = 100_000_000.0;
const SUNDAY: i64 = 3; // Weekday numbering is 0 = Thursday.
const LAST_MINUTE_OF_DAY: i64 = 1_439;

struct WeeklyVolatilityBudget {
    daily_returns: VecDeque<f64>,
    last_daily_close: Option<f64>,
    target_qty: Option<Qty>,
}

impl WeeklyVolatilityBudget {
    fn record_daily_close(&mut self, close: f64) {
        if let Some(previous) = self.last_daily_close {
            if previous > 0.0 && close > 0.0 {
                self.daily_returns.push_back((close / previous).ln());
                if self.daily_returns.len() > RETURNS_WINDOW {
                    self.daily_returns.pop_front();
                }
            }
        }
        self.last_daily_close = Some(close);
    }

    fn annualized_volatility(&self) -> Option<f64> {
        if self.daily_returns.len() != RETURNS_WINDOW {
            return None;
        }

        let count = self.daily_returns.len() as f64;
        let mean = self.daily_returns.iter().sum::<f64>() / count;
        let squared_deviations = self
            .daily_returns
            .iter()
            .map(|r| {
                let deviation = *r - mean;
                deviation * deviation
            })
            .sum::<f64>();
        let sample_variance = squared_deviations / (count - 1.0);
        Some(sample_variance.sqrt() * 365.0_f64.sqrt())
    }

    fn target_quantity(equity: f64, price: f64, weight: f64) -> Qty {
        if equity <= 0.0 || price <= 0.0 {
            return Qty::from_raw(0);
        }
        let btc = equity * weight / price;
        if !btc.is_finite() || btc <= 0.0 {
            return Qty::from_raw(0);
        }
        Qty::from_raw((btc * RAW_SCALE).floor() as i64)
    }

    fn submit_rebalance(&self, ctx: &mut Ctx, bar: &Bar) {
        let Some(target) = self.target_qty else {
            return;
        };
        if ctx.open_orders() != 0 {
            return;
        }

        // Submit no more than 10% of the latest completed bar's volume per attempt.
        let volume_cap = bar.volume.mul_ratio(1, 10);
        if !volume_cap.is_positive() {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if target > position {
            let needed = target - position;
            let mut qty = if needed < volume_cap {
                needed
            } else {
                volume_cap
            };
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if affordable < qty {
                qty = affordable;
            }
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        } else if position > target {
            let needed = position - target;
            let mut qty = if needed < volume_cap {
                needed
            } else {
                volume_cap
            };
            // Never submit a sell larger than the currently held position.
            if position < qty {
                qty = position;
            }
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
            }
        }
    }
}

impl Strategy for WeeklyVolatilityBudget {
    fn name(&self) -> String {
        "weekly-volatility-budget".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let minute_of_day = (bar.close_ts.as_nanos() / minute_ns) % 1_440;
        let day_of_week = (bar.close_ts.as_nanos() / day_ns) % 7;
        let is_daily_close = minute_of_day == LAST_MINUTE_OF_DAY;

        if is_daily_close {
            self.record_daily_close(bar.close.to_f64());

            if day_of_week == SUNDAY {
                if let Some(volatility) = self.annualized_volatility() {
                    let raw_weight = if volatility > 0.0 {
                        0.50 / volatility
                    } else {
                        1.00
                    };
                    let weight = raw_weight.clamp(0.25, 1.00);
                    self.target_qty = Some(Self::target_quantity(
                        ctx.equity().to_f64(),
                        bar.close.to_f64(),
                        weight,
                    ));
                }
            }
        }

        // Market orders submitted at the Sunday close are intended for the next
        // bar's open. IOC attempts on later bars carry any unfilled quantity forward.
        self.submit_rebalance(ctx, bar);
    }
}

export_strategy!(WeeklyVolatilityBudget {
    daily_returns: VecDeque::with_capacity(RETURNS_WINDOW + 1),
    last_daily_close: None,
    target_qty: None,
});
