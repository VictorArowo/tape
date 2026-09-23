use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const RETURN_WINDOW: usize = 10_080;
const MINUTES_PER_YEAR: f64 = 525_600.0;
const TARGET_VOLATILITY: f64 = 0.60;

struct VolatilityScaledExposure {
    previous_close: Option<f64>,
    returns: VecDeque<f64>,
}

impl VolatilityScaledExposure {
    fn new() -> Self {
        Self {
            previous_close: None,
            returns: VecDeque::with_capacity(RETURN_WINDOW + 1),
        }
    }

    fn record_close(&mut self, close: f64) {
        if let Some(previous) = self.previous_close {
            if previous > 0.0 && close > 0.0 {
                self.returns.push_back((close / previous).ln());
                if self.returns.len() > RETURN_WINDOW {
                    self.returns.pop_front();
                }
            }
        }
        self.previous_close = Some(close);
    }

    fn target_weight(&self) -> Option<f64> {
        if self.returns.len() < RETURN_WINDOW {
            return None;
        }

        let count = self.returns.len() as f64;
        let mean = self.returns.iter().sum::<f64>() / count;
        let variance = self
            .returns
            .iter()
            .map(|ret| {
                let deviation = ret - mean;
                deviation * deviation
            })
            .sum::<f64>()
            / count;
        let annualized_volatility = variance.sqrt() * MINUTES_PER_YEAR.sqrt();

        let weight = if annualized_volatility > 0.0 {
            (TARGET_VOLATILITY / annualized_volatility).min(1.0)
        } else {
            1.0
        };
        Some(weight)
    }
}

impl Strategy for VolatilityScaledExposure {
    fn name(&self) -> String {
        "volatility-scaled-exposure".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT || bar.close.to_f64() <= 0.0 {
            return;
        }

        // Record this completed bar before making the weekly estimate.
        self.record_close(bar.close.to_f64());

        let minute = (bar.close_ts.as_nanos()
            / tape_guest::TimeDelta::from_mins(1).as_nanos())
            % 1_440;
        let day_of_week = (bar.close_ts.as_nanos()
            / tape_guest::TimeDelta::from_days(1).as_nanos())
            % 7;
        // The SDK numbers Thursday as 0, so Monday is 4.
        if day_of_week != 4 || minute != 0 || ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        let Some(weight) = self.target_weight() else {
            // Until a full return window is available, keep the portfolio in cash.
            if position.is_positive() {
                let cap = bar.volume.mul_ratio(1, 10);
                let qty = if position < cap { position } else { cap };
                if qty.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
                }
            }
            return;
        };

        let target_value = ctx.equity().to_f64() * weight;
        let target_raw = (target_value / bar.close.to_f64() * 100_000_000.0) as i64;
        let target_qty = Qty::from_raw(target_raw.max(0));
        let cap = bar.volume.mul_ratio(1, 10);

        if position > target_qty {
            let difference = position - target_qty;
            let qty = if difference < cap { difference } else { cap };
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
            }
        } else if target_qty > position {
            let difference = target_qty - position;
            let volume_limited = if difference < cap { difference } else { cap };
            let affordable = ctx.affordable_qty(INSTRUMENT, bar.close, ctx.cash(), 5_000);
            let qty = if volume_limited < affordable {
                volume_limited
            } else {
                affordable
            };
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        }
    }
}

export_strategy!(VolatilityScaledExposure::new());
