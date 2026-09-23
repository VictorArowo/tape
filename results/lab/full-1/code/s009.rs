use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, TimeInForce,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const RETURN_WINDOW: usize = 10_080;
const ANNUAL_MINUTES: f64 = 525_600.0;
const TARGET_VOL: f64 = 0.50;
const REBALANCE_THRESHOLD: f64 = 0.05;
const MAX_BAR_VOLUME_FRACTION_NUM: i64 = 1;
const MAX_BAR_VOLUME_FRACTION_DEN: i64 = 10;
const SIZING_MARGIN_PPM: i64 = 5_000;
const SCALE: f64 = 100_000_000.0;

struct VolatilityTargetedBtc {
    previous_close: Option<f64>,
    squared_returns: VecDeque<f64>,
    squared_return_sum: f64,
}

impl VolatilityTargetedBtc {
    fn new() -> Self {
        Self {
            previous_close: None,
            squared_returns: VecDeque::with_capacity(RETURN_WINDOW + 1),
            squared_return_sum: 0.0,
        }
    }

    fn record_close(&mut self, close: f64) {
        if !close.is_finite() || close <= 0.0 {
            self.previous_close = None;
            self.squared_returns.clear();
            self.squared_return_sum = 0.0;
            return;
        }

        if let Some(previous) = self.previous_close {
            let r = close / previous - 1.0;
            let squared = r * r;
            if squared.is_finite() {
                self.squared_returns.push_back(squared);
                self.squared_return_sum += squared;
                if self.squared_returns.len() > RETURN_WINDOW {
                    if let Some(old) = self.squared_returns.pop_front() {
                        self.squared_return_sum -= old;
                    }
                }
            }
        }
        self.previous_close = Some(close);
    }

    fn target_weight(&self) -> Option<f64> {
        if self.squared_returns.len() != RETURN_WINDOW {
            return None;
        }
        let mean_squared_return = self.squared_return_sum / RETURN_WINDOW as f64;
        let annualized_volatility = (ANNUAL_MINUTES * mean_squared_return).sqrt();
        if !annualized_volatility.is_finite() {
            Some(0.0)
        } else if annualized_volatility <= 0.0 {
            Some(1.0)
        } else {
            Some((TARGET_VOL / annualized_volatility).min(1.0))
        }
    }

    fn is_day_end(bar: &Bar) -> bool {
        let minute = bar.close_ts.as_nanos() / TimeDelta::from_mins(1).as_nanos();
        minute.rem_euclid(1_440) == 1_439
    }

    fn submit_sell(&self, ctx: &mut Ctx, position: Qty, volume_cap: Qty) {
        if !position.is_positive() || !volume_cap.is_positive() {
            return;
        }
        let qty_raw = position.raw().min(volume_cap.raw());
        if qty_raw > 0 {
            ctx.submit(
                OrderRequest::market(INSTRUMENT, Side::Sell, Qty::from_raw(qty_raw))
                    .with_time_in_force(TimeInForce::Ioc),
            );
        }
    }
}

impl Strategy for VolatilityTargetedBtc {
    fn name(&self) -> String {
        "volatility-targeted-btc".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        self.record_close(bar.close.to_f64());
        if !Self::is_day_end(bar) || ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        let volume_cap = bar
            .volume
            .mul_ratio(MAX_BAR_VOLUME_FRACTION_NUM, MAX_BAR_VOLUME_FRACTION_DEN);
        let Some(target_weight) = self.target_weight() else {
            Self::submit_sell(self, ctx, position, volume_cap);
            return;
        };

        if target_weight < REBALANCE_THRESHOLD {
            Self::submit_sell(self, ctx, position, volume_cap);
            return;
        }

        let equity = ctx.equity();
        let close = bar.close.to_f64();
        if equity.to_f64() <= 0.0 || !close.is_finite() || close <= 0.0 {
            Self::submit_sell(self, ctx, position, volume_cap);
            return;
        }

        let position_value = (bar.close * position).to_f64();
        let current_weight = position_value / equity.to_f64();
        if !current_weight.is_finite()
            || (target_weight - current_weight).abs() < REBALANCE_THRESHOLD
        {
            return;
        }

        let desired_qty_raw_f = equity.to_f64() * target_weight / close * SCALE;
        if !desired_qty_raw_f.is_finite() || desired_qty_raw_f <= 0.0 {
            return;
        }
        let desired_qty_raw = desired_qty_raw_f.floor() as i64;
        if desired_qty_raw <= 0 {
            return;
        }

        if target_weight > current_weight {
            let delta_raw = desired_qty_raw.saturating_sub(position.raw());
            if delta_raw <= 0 || !volume_cap.is_positive() {
                return;
            }
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            let order_raw = delta_raw.min(affordable.raw()).min(volume_cap.raw());
            if order_raw > 0 {
                ctx.submit(
                    OrderRequest::market(INSTRUMENT, Side::Buy, Qty::from_raw(order_raw))
                        .with_time_in_force(TimeInForce::Ioc),
                );
            }
        } else {
            let delta_raw = position.raw().saturating_sub(desired_qty_raw);
            if delta_raw <= 0 || !volume_cap.is_positive() {
                return;
            }
            let order_raw = delta_raw.min(position.raw()).min(volume_cap.raw());
            if order_raw > 0 {
                ctx.submit(
                    OrderRequest::market(INSTRUMENT, Side::Sell, Qty::from_raw(order_raw))
                        .with_time_in_force(TimeInForce::Ioc),
                );
            }
        }
    }
}

export_strategy!(VolatilityTargetedBtc::new());
