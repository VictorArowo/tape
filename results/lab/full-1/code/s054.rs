use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const EWMA_LAMBDA: f64 = 0.94;
const VOLATILITY_TARGET: f64 = 0.70;
const MIN_EXPOSURE: f64 = 0.25;
const MAX_EXPOSURE: f64 = 1.0;
const REBALANCE_THRESHOLD: f64 = 0.05;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct EwmaVolTarget {
    previous_daily_close: Option<f64>,
    ewma_variance: Option<f64>,
    last_processed_day: Option<i64>,
}

impl EwmaVolTarget {
    fn finish_daily_close(&mut self, day: i64, close: f64) -> Option<f64> {
        if self.last_processed_day == Some(day) || close <= 0.0 {
            return None;
        }
        self.last_processed_day = Some(day);

        let previous = self.previous_daily_close.replace(close);
        let previous = previous?;
        let daily_return = (close / previous).ln();
        let squared_return = daily_return * daily_return;

        let variance = match self.ewma_variance {
            Some(old) => EWMA_LAMBDA * old + (1.0 - EWMA_LAMBDA) * squared_return,
            None => squared_return,
        };
        self.ewma_variance = Some(variance);

        let annualized_volatility = (365.0 * variance).sqrt();
        let target = if annualized_volatility <= 0.0 {
            MAX_EXPOSURE
        } else {
            (VOLATILITY_TARGET / annualized_volatility).clamp(MIN_EXPOSURE, MAX_EXPOSURE)
        };
        Some(target)
    }

    fn positive_qty_from_units(units: f64) -> Option<Qty> {
        if !units.is_finite() || units <= 0.0 {
            return None;
        }
        let raw = (units * 1e8).floor();
        if raw < 1.0 || raw >= i64::MAX as f64 {
            return None;
        }
        Some(Qty::from_raw(raw as i64))
    }

    fn rebalance(&self, ctx: &mut Ctx, bar: &Bar, target_weight: f64) {
        if ctx.open_orders() != 0 || !bar.close.is_positive() {
            return;
        }

        let equity = ctx.equity().to_f64();
        if !equity.is_finite() || equity <= 0.0 {
            return;
        }

        let price = bar.close.to_f64();
        let position = ctx.position(INSTRUMENT);
        let position_units = position.to_f64();
        let actual_weight = position_units * price / equity;
        if (target_weight - actual_weight).abs() < REBALANCE_THRESHOLD {
            return;
        }

        let desired_units = equity * target_weight / price - position_units;
        let Some(requested_qty) = Self::positive_qty_from_units(desired_units.abs()) else {
            return;
        };

        if desired_units > 0.0 {
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            let raw = requested_qty.raw().min(affordable.raw());
            if raw > 0 {
                ctx.submit(OrderRequest::market(
                    INSTRUMENT,
                    Side::Buy,
                    Qty::from_raw(raw),
                ));
            }
        } else if position.is_positive() {
            let raw = requested_qty.raw().min(position.raw());
            if raw > 0 {
                ctx.submit(OrderRequest::market(
                    INSTRUMENT,
                    Side::Sell,
                    Qty::from_raw(raw),
                ));
            }
        }
    }
}

impl Strategy for EwmaVolTarget {
    fn name(&self) -> String {
        "ewma-volatility-target".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        // The 23:59 UTC one-minute bar's close is the completed daily close.
        // Orders submitted here are eligible for execution at the next bar's open.
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        if minute_ns <= 0 {
            return;
        }
        let minute_index = bar.open_ts.as_nanos().div_euclid(minute_ns);
        if minute_index.rem_euclid(1440) != 1439 {
            return;
        }

        let day = minute_index.div_euclid(1440);
        let Some(target_weight) = self.finish_daily_close(day, bar.close.to_f64()) else {
            return;
        };
        self.rebalance(ctx, bar, target_weight);
    }
}

export_strategy!(EwmaVolTarget {
    previous_daily_close: None,
    ewma_variance: None,
    last_processed_day: None,
});
