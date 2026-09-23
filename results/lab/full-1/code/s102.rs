use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const RETURN_COUNT: usize = 336;
const SIZING_MARGIN_PPM: i64 = 5_000;
const SCALE: i128 = 100_000_000;

struct DailyVolatilityBudget {
    hourly_returns: VecDeque<f64>,
    previous_hour_close: Option<(i64, i64)>,
    last_signal_day: Option<i64>,
    target_qty_raw: i64,
    rebalance_pending: bool,
}

impl DailyVolatilityBudget {
    fn new() -> Self {
        Self {
            hourly_returns: VecDeque::with_capacity(RETURN_COUNT + 1),
            previous_hour_close: None,
            last_signal_day: None,
            target_qty_raw: 0,
            rebalance_pending: false,
        }
    }

    fn record_hourly_close(&mut self, bar: &Bar) {
        let hour_ns = TimeDelta::from_mins(60).as_nanos();
        let ts = bar.close_ts.as_nanos();
        if ts.rem_euclid(hour_ns) != 0 {
            return;
        }

        if let Some((previous_ts, previous_close)) = self.previous_hour_close {
            if ts - previous_ts == hour_ns && previous_close > 0 && bar.close.raw() > 0 {
                let hourly_return = bar.close.raw() as f64 / previous_close as f64 - 1.0;
                self.hourly_returns.push_back(hourly_return);
                if self.hourly_returns.len() > RETURN_COUNT {
                    self.hourly_returns.pop_front();
                }
            } else {
                self.hourly_returns.clear();
            }
        }
        self.previous_hour_close = Some((ts, bar.close.raw()));
    }

    fn annualized_volatility(&self) -> Option<f64> {
        if self.hourly_returns.len() != RETURN_COUNT {
            return None;
        }
        let count = RETURN_COUNT as f64;
        let mean = self.hourly_returns.iter().copied().sum::<f64>() / count;
        let variance = self
            .hourly_returns
            .iter()
            .map(|r| {
                let difference = *r - mean;
                difference * difference
            })
            .sum::<f64>()
            / count;
        Some(variance.max(0.0).sqrt() * 8_760.0_f64.sqrt())
    }

    fn target_quantity(nav_raw: i64, price: Price, weight: f64) -> i64 {
        if nav_raw <= 0 || price.raw() <= 0 {
            return 0;
        }
        let target_value_raw = (nav_raw as f64 * weight).floor() as i128;
        let quantity_raw = target_value_raw * SCALE / price.raw() as i128;
        quantity_raw.clamp(0, i64::MAX as i128) as i64
    }

    fn update_daily_target(&mut self, ctx: &Ctx, bar: &Bar) {
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let ts = bar.close_ts.as_nanos();
        if ts.rem_euclid(day_ns) != 0 {
            return;
        }
        let day = ts.div_euclid(day_ns);
        if self.last_signal_day == Some(day) {
            return;
        }
        self.last_signal_day = Some(day);

        let Some(volatility) = self.annualized_volatility() else {
            return;
        };
        let nav = ctx.equity();
        if nav.raw() <= 0 || bar.close.raw() <= 0 {
            return;
        }

        let target_weight = if volatility > 0.0 && volatility.is_finite() {
            (0.60 / volatility).clamp(0.20, 1.00)
        } else {
            1.00
        };
        let target_value_raw = (nav.raw() as f64 * target_weight).floor() as i128;
        let position = ctx.position(INSTRUMENT);
        let actual_value_raw = if position.is_positive() {
            (bar.close * position).raw() as i128
        } else {
            0
        };
        let difference = (target_value_raw - actual_value_raw).abs();
        let minimum_difference = (nav.raw() / 10) as i128;

        if difference >= minimum_difference {
            self.target_qty_raw = Self::target_quantity(nav.raw(), bar.close, target_weight);
            self.rebalance_pending = true;
        }
    }

    fn work_toward_target(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if !self.rebalance_pending {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        let position_raw = position.raw().max(0);
        let difference = self.target_qty_raw as i128 - position_raw as i128;
        if difference == 0 {
            self.rebalance_pending = false;
            return;
        }
        if ctx.open_orders() != 0 || bar.close.raw() <= 0 {
            return;
        }

        if difference > 0 {
            let requested_raw = difference.min(i64::MAX as i128) as i64;
            let requested = tape_guest::Qty::from_raw(requested_raw);
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            let quantity = if affordable.raw() < requested.raw() {
                affordable
            } else {
                requested
            };
            if quantity.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, quantity));
            }
        } else {
            let sell_raw = (-difference).min(position_raw as i128) as i64;
            if sell_raw > 0 {
                ctx.submit(OrderRequest::market(
                    INSTRUMENT,
                    Side::Sell,
                    tape_guest::Qty::from_raw(sell_raw),
                ));
            }
        }
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
        self.record_hourly_close(bar);
        self.update_daily_target(ctx, bar);
        self.work_toward_target(ctx, bar);
    }
}

export_strategy!(DailyVolatilityBudget::new());
