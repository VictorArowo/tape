use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const RETURN_WINDOW: usize = 10_080;
const TARGET_VOLATILITY: f64 = 0.60;
const MIN_ALLOCATION: f64 = 0.25;
const MAX_ALLOCATION: f64 = 1.00;
const REBALANCE_THRESHOLD: f64 = 0.05;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct DailyVolatilityBudget {
    previous_close: Option<i64>,
    squared_returns: VecDeque<f64>,
    sum_squared_returns: f64,
}

impl DailyVolatilityBudget {
    fn new() -> Self {
        Self {
            previous_close: None,
            squared_returns: VecDeque::with_capacity(RETURN_WINDOW + 1),
            sum_squared_returns: 0.0,
        }
    }

    fn push_close(&mut self, close: i64) {
        if let Some(previous) = self.previous_close {
            if previous > 0 && close > 0 {
                let log_return = (close as f64 / previous as f64).ln();
                let squared_return = log_return * log_return;
                self.squared_returns.push_back(squared_return);
                self.sum_squared_returns += squared_return;

                if self.squared_returns.len() > RETURN_WINDOW {
                    if let Some(removed) = self.squared_returns.pop_front() {
                        self.sum_squared_returns -= removed;
                    }
                }
            }
        }
        self.previous_close = Some(close);
    }

    fn target_allocation(&self) -> Option<f64> {
        if self.squared_returns.len() != RETURN_WINDOW {
            return None;
        }

        let sigma = (self.sum_squared_returns * (365.0 / 7.0)).sqrt();
        let allocation = if sigma == 0.0 {
            MAX_ALLOCATION
        } else {
            (TARGET_VOLATILITY / sigma).clamp(MIN_ALLOCATION, MAX_ALLOCATION)
        };
        Some(allocation)
    }
}

impl Strategy for DailyVolatilityBudget {
    fn name(&self) -> String {
        "daily-volatility-budget".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        self.push_close(bar.close.raw());

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let minute_of_day = (bar.close_ts.as_nanos() / minute_ns) % 1_440;
        if minute_of_day != 0 {
            return;
        }

        let Some(target_allocation) = self.target_allocation() else {
            return;
        };
        if ctx.open_orders() != 0 {
            return;
        }

        let equity = ctx.equity();
        let equity_value = equity.to_f64();
        let price_value = bar.close.to_f64();
        if !equity_value.is_finite()
            || equity_value <= 0.0
            || !price_value.is_finite()
            || price_value <= 0.0
        {
            return;
        }

        let held = ctx.position(INSTRUMENT);
        if held.raw() < 0 {
            return;
        }
        let held_value = (bar.close * held).to_f64();
        let current_allocation = held_value / equity_value;
        if !current_allocation.is_finite()
            || (target_allocation - current_allocation).abs() < REBALANCE_THRESHOLD
        {
            return;
        }

        let target_qty_raw = (equity_value * target_allocation / price_value) * 100_000_000.0;
        if !target_qty_raw.is_finite() || target_qty_raw <= 0.0 || target_qty_raw >= i64::MAX as f64
        {
            return;
        }
        let target_qty = Qty::from_raw(target_qty_raw as i64);

        if target_qty > held {
            let desired_buy = Qty::from_raw(target_qty.raw() - held.raw());
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            let buy_qty = if desired_buy < affordable {
                desired_buy
            } else {
                affordable
            };
            if buy_qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, buy_qty));
            }
        } else if target_qty < held {
            let sell_qty = Qty::from_raw((held.raw() - target_qty.raw()).min(held.raw()));
            if sell_qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, sell_qty));
            }
        }
    }
}

export_strategy!(DailyVolatilityBudget::new());
