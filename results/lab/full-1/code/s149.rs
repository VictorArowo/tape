use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const RETURN_WINDOW: usize = 7;
const REBALANCE_THRESHOLD: f64 = 0.05;
const SIZING_MARGIN_PPM: i64 = 5_000;
const QTY_SCALE: f64 = 100_000_000.0;

struct InverseVolatility {
    daily_returns: VecDeque<f64>,
    previous_daily_close: Option<f64>,
    previous_daily_day: Option<i64>,
}

impl InverseVolatility {
    fn new() -> Self {
        Self {
            daily_returns: VecDeque::with_capacity(RETURN_WINDOW),
            previous_daily_close: None,
            previous_daily_day: None,
        }
    }

    fn record_daily_close(&mut self, close: f64, day: i64) -> Option<f64> {
        if !close.is_finite() || close <= 0.0 {
            self.daily_returns.clear();
            self.previous_daily_close = None;
            self.previous_daily_day = Some(day);
            return None;
        }

        let consecutive = self
            .previous_daily_day
            .and_then(|previous_day| previous_day.checked_add(1))
            == Some(day);

        if consecutive {
            if let Some(previous_close) = self.previous_daily_close {
                let daily_return = (close / previous_close).ln();
                if daily_return.is_finite() {
                    self.daily_returns.push_back(daily_return);
                    if self.daily_returns.len() > RETURN_WINDOW {
                        self.daily_returns.pop_front();
                    }
                } else {
                    self.daily_returns.clear();
                }
            } else {
                self.daily_returns.clear();
            }
        } else {
            self.daily_returns.clear();
        }

        self.previous_daily_close = Some(close);
        self.previous_daily_day = Some(day);

        if self.daily_returns.len() != RETURN_WINDOW {
            return None;
        }

        let mean = self.daily_returns.iter().sum::<f64>() / RETURN_WINDOW as f64;
        let variance = self
            .daily_returns
            .iter()
            .map(|value| {
                let difference = value - mean;
                difference * difference
            })
            .sum::<f64>()
            / RETURN_WINDOW as f64;
        let annualized_volatility = variance.sqrt() * 365.0_f64.sqrt();

        if !annualized_volatility.is_finite() {
            return None;
        }

        let target_weight = if annualized_volatility <= f64::EPSILON {
            1.0
        } else {
            (0.60 / annualized_volatility).min(1.0)
        };
        Some(target_weight.clamp(0.0, 1.0))
    }

    fn qty_from_btc(value: f64) -> Qty {
        if !value.is_finite() || value <= 0.0 {
            return Qty::from_raw(0);
        }
        let raw = value * QTY_SCALE;
        if !raw.is_finite() || raw <= 0.0 {
            return Qty::from_raw(0);
        }
        Qty::from_raw(raw.floor().min(i64::MAX as f64) as i64)
    }
}

impl Strategy for InverseVolatility {
    fn name(&self) -> String {
        "inverse-volatility-exposure(7d,60%)".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let day_ns = TimeDelta::from_days(1).as_nanos();
        if day_ns <= 0 {
            return;
        }

        let open_ns = bar.open_ts.as_nanos();
        let close_ns = bar.close_ts.as_nanos();
        let open_day = open_ns.div_euclid(day_ns);
        let close_day = close_ns.div_euclid(day_ns);

        // A bar crossing UTC midnight supplies that completed day's close.
        if open_day == close_day {
            return;
        }

        let Some(target_weight) = self.record_daily_close(bar.close.to_f64(), open_day) else {
            return;
        };

        if ctx.open_orders() != 0 {
            return;
        }

        let price = bar.close.to_f64();
        let nav = ctx.equity().to_f64();
        if !price.is_finite() || price <= 0.0 || !nav.is_finite() || nav <= 0.0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if !position.is_zero() && !position.is_positive() {
            return;
        }

        let current_btc_value = position.to_f64() * price;
        let current_weight = current_btc_value / nav;
        if !current_weight.is_finite()
            || (target_weight - current_weight).abs() < REBALANCE_THRESHOLD
        {
            return;
        }

        let target_btc_value = nav * target_weight;
        if target_btc_value > current_btc_value {
            let desired_qty = Self::qty_from_btc((target_btc_value - current_btc_value) / price);
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            let qty = if desired_qty < affordable {
                desired_qty
            } else {
                affordable
            };
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        } else {
            let desired_qty = Self::qty_from_btc((current_btc_value - target_btc_value) / price);
            let qty = if desired_qty < position {
                desired_qty
            } else {
                position
            };
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
            }
        }
    }
}

export_strategy!(InverseVolatility::new());