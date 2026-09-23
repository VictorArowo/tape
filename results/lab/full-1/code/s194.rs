use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const RETURNS_WINDOW: usize = 336;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct InverseVarianceRiskBudget {
    hourly_returns: VecDeque<f64>,
    last_hour: Option<i64>,
    last_hour_close: Option<f64>,
    last_rebalance_ts: Option<i64>,
}

impl InverseVarianceRiskBudget {
    fn new() -> Self {
        Self {
            hourly_returns: VecDeque::with_capacity(RETURNS_WINDOW),
            last_hour: None,
            last_hour_close: None,
            last_rebalance_ts: None,
        }
    }

    fn record_hourly_close(&mut self, bar: &Bar) {
        let ts = bar.close_ts.as_nanos();
        let hour_ns = TimeDelta::from_hours(1).as_nanos();
        if hour_ns <= 0 || ts.rem_euclid(hour_ns) != 0 {
            return;
        }

        let hour = ts.div_euclid(hour_ns);
        if let Some(previous_hour) = self.last_hour {
            if hour <= previous_hour {
                return;
            }
            if hour != previous_hour.saturating_add(1) {
                self.hourly_returns.clear();
            }
        }

        let close = bar.close.to_f64();
        if let Some(previous_close) = self.last_hour_close {
            if self.last_hour.is_some()
                && hour == self.last_hour.unwrap().saturating_add(1)
                && close.is_finite()
                && previous_close.is_finite()
                && close > 0.0
                && previous_close > 0.0
            {
                let ret = (close / previous_close).ln();
                if ret.is_finite() {
                    self.hourly_returns.push_back(ret);
                    if self.hourly_returns.len() > RETURNS_WINDOW {
                        self.hourly_returns.pop_front();
                    }
                } else {
                    self.hourly_returns.clear();
                }
            }
        }

        self.last_hour = Some(hour);
        self.last_hour_close = (close.is_finite() && close > 0.0).then_some(close);
    }

    fn annualized_volatility(&self) -> Option<f64> {
        if self.hourly_returns.len() != RETURNS_WINDOW {
            return None;
        }

        let count = self.hourly_returns.len() as f64;
        let mean = self.hourly_returns.iter().sum::<f64>() / count;
        let squared_deviations = self
            .hourly_returns
            .iter()
            .map(|ret| {
                let deviation = ret - mean;
                deviation * deviation
            })
            .sum::<f64>();
        // Use the sample standard deviation of the rolling return window.
        let variance = squared_deviations / (count - 1.0);
        let volatility = variance.sqrt() * 8_760.0_f64.sqrt();
        volatility.is_finite().then_some(volatility)
    }
}

impl Strategy for InverseVarianceRiskBudget {
    fn name(&self) -> String {
        "Inverse-Variance Risk Budget [wasm]".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        self.record_hourly_close(bar);

        let ts = bar.close_ts.as_nanos();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        if minute_ns <= 0 || day_ns <= 0 {
            return;
        }
        let minute_of_day = (ts / minute_ns) % 1_440;
        let day_of_week = (ts / day_ns) % 7;
        // Unix day zero is Thursday, so Monday is day 4.
        if minute_of_day != 0 || day_of_week != 4 || self.last_rebalance_ts == Some(ts) {
            return;
        }
        self.last_rebalance_ts = Some(ts);

        let Some(volatility) = self.annualized_volatility() else {
            return;
        };
        if ctx.open_orders() != 0 {
            return;
        }

        let target_weight = if volatility <= 0.0 {
            1.0
        } else {
            (0.70 / volatility).powi(2).min(1.0)
        };
        if !target_weight.is_finite() {
            return;
        }

        let equity = ctx.equity().to_f64();
        let price = bar.close.to_f64();
        if !equity.is_finite() || !price.is_finite() || equity <= 0.0 || price <= 0.0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        let current_weight = position.to_f64() * price / equity;
        if !current_weight.is_finite() || (target_weight - current_weight).abs() < 0.05 {
            return;
        }

        let target_qty_raw = target_weight * equity / price * 1e8;
        if !target_qty_raw.is_finite() || target_qty_raw < 0.0 || target_qty_raw > i64::MAX as f64 {
            return;
        }
        let target_qty = Qty::from_raw(target_qty_raw as i64);
        let position_raw = position.raw();

        if target_qty.raw() > position_raw {
            let requested_raw = target_qty.raw() - position_raw;
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            let qty = Qty::from_raw(requested_raw.min(affordable.raw()).max(0));
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        } else if position.is_positive() {
            let sell_raw = (position_raw - target_qty.raw()).max(0).min(position_raw);
            let qty = Qty::from_raw(sell_raw);
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
            }
        }
    }
}

export_strategy!(InverseVarianceRiskBudget::new());
