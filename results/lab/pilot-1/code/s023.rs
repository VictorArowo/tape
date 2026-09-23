use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const HOURLY_WINDOW: usize = 336;
const HISTORY_DAYS: usize = 180;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct DownsideSemivolatilityGate {
    previous_hour_close: Option<f64>,
    hourly_returns: VecDeque<f64>,
    historical_semivols: VecDeque<f64>,
    target_fraction: f64,
    target_qty_raw: i64,
    rebalance_active: bool,
}

impl DownsideSemivolatilityGate {
    fn new() -> Self {
        Self {
            previous_hour_close: None,
            hourly_returns: VecDeque::with_capacity(HOURLY_WINDOW + 1),
            historical_semivols: VecDeque::with_capacity(HISTORY_DAYS + 1),
            target_fraction: 1.0,
            target_qty_raw: 0,
            rebalance_active: false,
        }
    }

    fn downside_semivolatility(&self) -> Option<f64> {
        if self.hourly_returns.len() < HOURLY_WINDOW {
            return None;
        }

        let negative: Vec<f64> = self
            .hourly_returns
            .iter()
            .copied()
            .filter(|value| *value < 0.0)
            .collect();
        if negative.is_empty() {
            return Some(0.0);
        }

        let mean = negative.iter().sum::<f64>() / negative.len() as f64;
        let variance = negative
            .iter()
            .map(|value| {
                let delta = value - mean;
                delta * delta
            })
            .sum::<f64>()
            / negative.len() as f64;
        Some(variance.sqrt())
    }

    fn percentile_75(values: &VecDeque<f64>) -> Option<f64> {
        if values.len() < HISTORY_DAYS {
            return None;
        }
        let mut sorted: Vec<f64> = values.iter().copied().collect();
        sorted.sort_by(f64::total_cmp);
        let index = ((sorted.len() - 1) * 3 + 3) / 4;
        sorted.get(index).copied()
    }

    fn set_target(&mut self, ctx: &Ctx, price_raw: i64) {
        if price_raw <= 0 {
            return;
        }
        let equity_raw = ctx.equity().raw().max(0);
        let target_value_raw = (equity_raw as f64 * self.target_fraction) as i128;
        let quantity_raw = target_value_raw * 100_000_000_i128 / i128::from(price_raw);
        self.target_qty_raw = quantity_raw.clamp(0, i128::from(i64::MAX)) as i64;
        self.rebalance_active = true;
    }

    fn attempt_rebalance(&mut self, ctx: &mut Ctx, price_raw: i64) {
        if !self.rebalance_active || ctx.open_orders() != 0 || price_raw <= 0 {
            return;
        }

        let position_raw = ctx.position(INSTRUMENT).raw().max(0);
        let difference = i128::from(self.target_qty_raw) - i128::from(position_raw);
        if difference == 0 {
            self.rebalance_active = false;
            return;
        }

        if difference < 0 {
            let sell_raw = (-difference).min(i128::from(position_raw)) as i64;
            if sell_raw > 0 {
                ctx.submit(OrderRequest::market(
                    INSTRUMENT,
                    Side::Sell,
                    tape_guest::Qty::from_raw(sell_raw),
                ));
            }
        } else {
            let price = tape_guest::Price::from_raw(price_raw);
            let wanted = tape_guest::Qty::from_raw(difference.min(i128::from(i64::MAX)) as i64);
            let affordable = ctx.affordable_qty(INSTRUMENT, price, ctx.cash(), SIZING_MARGIN_PPM);
            let quantity = if wanted < affordable { wanted } else { affordable };
            if quantity.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, quantity));
            }
        }
    }
}

impl Strategy for DownsideSemivolatilityGate {
    fn name(&self) -> String {
        "Downside Semivolatility Gate".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let minute_of_hour = (bar.close_ts.as_nanos()
            / tape_guest::TimeDelta::from_mins(1).as_nanos())
            % 60;
        if minute_of_hour == 0 {
            let close = bar.close.to_f64();
            if close > 0.0 {
                if let Some(previous) = self.previous_hour_close {
                    if previous > 0.0 {
                        self.hourly_returns.push_back((close / previous).ln());
                        if self.hourly_returns.len() > HOURLY_WINDOW {
                            self.hourly_returns.pop_front();
                        }
                    }
                }
                self.previous_hour_close = Some(close);
            }
        }

        let minute_of_day = (bar.close_ts.as_nanos()
            / tape_guest::TimeDelta::from_mins(1).as_nanos())
            % 1440;
        if minute_of_day == 0 {
            if let Some(current) = self.downside_semivolatility() {
                if let Some(threshold) = Self::percentile_75(&self.historical_semivols) {
                    self.target_fraction = if current > threshold { 0.25 } else { 1.0 };
                }
                self.historical_semivols.push_back(current);
                if self.historical_semivols.len() > HISTORY_DAYS {
                    self.historical_semivols.pop_front();
                }
            }
            self.set_target(ctx, bar.close.raw());
        }

        // A daily target remains active until the position reaches it; retries
        // use the latest completed close and are suppressed while an order rests.
        self.attempt_rebalance(ctx, bar.close.raw());
    }
}

export_strategy!(DownsideSemivolatilityGate::new());
