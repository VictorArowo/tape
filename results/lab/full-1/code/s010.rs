use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, TimeInForce,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const RETURNS_NEEDED: usize = 336;
const ANNUAL_HOURS: f64 = 8760.0;
const VOLATILITY_BUDGET: f64 = 0.70;
const REBALANCE_THRESHOLD: f64 = 0.05;
const SIZING_MARGIN_PPM: i64 = 5_000;
const FIXED_POINT_SCALE: f64 = 100_000_000.0;

struct DailyVolatilityScaling {
    hourly_returns: VecDeque<f64>,
    last_hour_close: Option<f64>,
    last_hour_bucket: Option<i64>,
}

impl DailyVolatilityScaling {
    fn new() -> Self {
        Self {
            hourly_returns: VecDeque::with_capacity(RETURNS_NEEDED),
            last_hour_close: None,
            last_hour_bucket: None,
        }
    }

    fn record_hourly_close(&mut self, bar: &Bar) {
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let hour_ns = TimeDelta::from_mins(60).as_nanos();
        let ts_ns = bar.close_ts.as_nanos();
        let minute_of_hour = (ts_ns / minute_ns) % 60;
        if minute_of_hour != 0 {
            return;
        }

        let hour_bucket = ts_ns / hour_ns;
        if self.last_hour_bucket == Some(hour_bucket) {
            return;
        }
        self.last_hour_bucket = Some(hour_bucket);

        let close = bar.close.to_f64();
        if close <= 0.0 || !close.is_finite() {
            return;
        }

        if let Some(previous) = self.last_hour_close {
            if previous > 0.0 {
                let log_return = (close / previous).ln();
                if log_return.is_finite() {
                    self.hourly_returns.push_back(log_return);
                    if self.hourly_returns.len() > RETURNS_NEEDED {
                        self.hourly_returns.pop_front();
                    }
                }
            }
        }
        self.last_hour_close = Some(close);
    }

    fn annualized_volatility(&self) -> Option<f64> {
        if self.hourly_returns.len() != RETURNS_NEEDED {
            return None;
        }

        let mean = self.hourly_returns.iter().sum::<f64>() / RETURNS_NEEDED as f64;
        let variance = self
            .hourly_returns
            .iter()
            .map(|r| {
                let difference = *r - mean;
                difference * difference
            })
            .sum::<f64>()
            / RETURNS_NEEDED as f64;
        let annualized = variance.sqrt() * ANNUAL_HOURS.sqrt();
        annualized.is_finite().then_some(annualized)
    }

    fn rebalance(&self, ctx: &mut Ctx, price: tape_guest::Price) {
        if ctx.open_orders() != 0 {
            return;
        }

        let Some(sigma) = self.annualized_volatility() else {
            return;
        };
        let target_weight = if sigma <= 0.0 {
            1.0
        } else {
            (VOLATILITY_BUDGET / sigma).min(1.0)
        };

        let price_f64 = price.to_f64();
        let equity = ctx.equity();
        let equity_f64 = equity.to_f64();
        if price_f64 <= 0.0 || equity_f64 <= 0.0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        let current_weight = position.to_f64() * price_f64 / equity_f64;
        if (target_weight - current_weight).abs() < REBALANCE_THRESHOLD {
            return;
        }

        let target_qty_units = equity_f64 * target_weight / price_f64;
        if !target_qty_units.is_finite() || target_qty_units < 0.0 {
            return;
        }
        let target_qty = Qty::from_raw((target_qty_units * FIXED_POINT_SCALE).floor() as i64);

        if target_weight > current_weight {
            let delta_raw = target_qty.raw().saturating_sub(position.raw());
            if delta_raw <= 0 {
                return;
            }
            let desired = Qty::from_raw(delta_raw);
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                price,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            let buy_qty = Qty::from_raw(delta_raw.min(affordable.raw()));
            if buy_qty.is_positive() {
                let _ = desired;
                ctx.submit(
                    OrderRequest::market(INSTRUMENT, Side::Buy, buy_qty)
                        .with_time_in_force(TimeInForce::Ioc),
                );
            }
        } else if position.is_positive() {
            let sell_raw = position
                .raw()
                .saturating_sub(target_qty.raw())
                .min(position.raw());
            let sell_qty = Qty::from_raw(sell_raw);
            if sell_qty.is_positive() {
                ctx.submit(
                    OrderRequest::market(INSTRUMENT, Side::Sell, sell_qty)
                        .with_time_in_force(TimeInForce::Ioc),
                );
            }
        }
    }
}

impl Strategy for DailyVolatilityScaling {
    fn name(&self) -> String {
        "daily-volatility-scaling".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        self.record_hourly_close(bar);

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let minute_of_day = (bar.close_ts.as_nanos() / minute_ns) % 1440;
        if minute_of_day == 0 {
            self.rebalance(ctx, bar.close);
        }
    }
}

export_strategy!(DailyVolatilityScaling::new());
