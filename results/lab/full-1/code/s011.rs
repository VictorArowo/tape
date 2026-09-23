use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const LOOKBACK: usize = 1_440;
const ANNUAL_MINUTES: f64 = 525_600.0;
const RISK_BUDGET: f64 = 0.45;
const REBALANCE_BAND: f64 = 0.10;
const SIZING_MARGIN_PPM: i64 = 5_000;
const RAW_SCALE: f64 = 100_000_000.0;

struct DownsideVolatilityBudget {
    previous_close_raw: Option<i64>,
    downside_squares: VecDeque<f64>,
    downside_sum: f64,
}

impl DownsideVolatilityBudget {
    fn new() -> Self {
        Self {
            previous_close_raw: None,
            downside_squares: VecDeque::with_capacity(LOOKBACK + 1),
            downside_sum: 0.0,
        }
    }

    fn push_close(&mut self, close: Price) {
        let close_raw = close.raw();
        if let Some(previous_raw) = self.previous_close_raw {
            if previous_raw > 0 {
                let ret = close_raw as f64 / previous_raw as f64 - 1.0;
                let downside = ret.min(0.0);
                let square = downside * downside;
                self.downside_squares.push_back(square);
                self.downside_sum += square;

                if self.downside_squares.len() > LOOKBACK {
                    if let Some(old) = self.downside_squares.pop_front() {
                        self.downside_sum -= old;
                    }
                }
            }
        }
        self.previous_close_raw = Some(close_raw);
    }

    fn target_weight(&self) -> Option<f64> {
        if self.downside_squares.len() != LOOKBACK {
            return None;
        }
        let mean_square = (self.downside_sum.max(0.0)) / LOOKBACK as f64;
        let volatility = (ANNUAL_MINUTES * mean_square).sqrt();
        let weight = if volatility == 0.0 {
            1.0
        } else {
            (RISK_BUDGET / volatility).clamp(0.0, 1.0)
        };
        Some(weight)
    }

    fn rebalance(&self, ctx: &mut Ctx, bar: &Bar, target_weight: f64) {
        if ctx.open_orders() != 0 {
            return;
        }

        let equity = ctx.equity().to_f64();
        let price = bar.close.to_f64();
        if !equity.is_finite() || equity <= 0.0 || !price.is_finite() || price <= 0.0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        let actual_value = (bar.close * position).to_f64();
        let target_value = equity * target_weight;
        let difference = target_value - actual_value;
        if difference.abs() <= equity * REBALANCE_BAND {
            return;
        }

        let volume_cap = bar.volume.mul_ratio(1, 10);
        if !volume_cap.is_positive() {
            return;
        }

        if difference > 0.0 {
            let desired = Self::qty_for_value(difference, bar.close);
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            let qty = Self::min_qty(desired, Self::min_qty(affordable, volume_cap));
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        } else if position.is_positive() {
            let desired = Self::qty_for_value(-difference, bar.close);
            let qty = Self::min_qty(desired, Self::min_qty(position, volume_cap));
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
            }
        }
    }

    fn qty_for_value(value: f64, price: Price) -> Qty {
        let price_value = price.to_f64();
        if !value.is_finite() || value <= 0.0 || !price_value.is_finite() || price_value <= 0.0 {
            return Qty::from_raw(0);
        }
        let raw = (value / price_value * RAW_SCALE).floor();
        if raw <= 0.0 {
            Qty::from_raw(0)
        } else {
            Qty::from_raw(raw as i64)
        }
    }

    fn min_qty(a: Qty, b: Qty) -> Qty {
        Qty::from_raw(a.raw().min(b.raw()).max(0))
    }
}

impl Strategy for DownsideVolatilityBudget {
    fn name(&self) -> String {
        "downside-volatility-budget".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        // Each close contributes one completed close-to-close return.
        self.push_close(bar.close);

        let minute_ns = tape_guest::TimeDelta::from_mins(1).as_nanos();
        if minute_ns <= 0 || (bar.close_ts.as_nanos() / minute_ns) % 240 != 0 {
            return;
        }

        // No exposure is targeted until all 1,440 returns are available.
        let target_weight = self.target_weight().unwrap_or(0.0);
        self.rebalance(ctx, bar, target_weight);
    }
}

export_strategy!(DownsideVolatilityBudget::new());
