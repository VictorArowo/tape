use tape_guest::{
    export_strategy, Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy,
    TimeDelta, TimeInForce,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const SCALE: i128 = 100_000_000;

struct FixedWeightVolatilityHarvest {
    reference: Option<Price>,
    order_active: bool,
    fill_notional: i128,
    fill_qty: i128,
}

impl FixedWeightVolatilityHarvest {
    fn target_order(ctx: &Ctx, close: Price) -> (Option<(Side, Qty)>, bool) {
        if close.raw() <= 0 {
            return (None, false);
        }

        let equity_raw = i128::from(ctx.equity().raw()).max(0);
        let target_value_raw = equity_raw * 9 / 10;
        let target_qty_raw = (target_value_raw * SCALE / i128::from(close.raw()))
            .clamp(0, i128::from(i64::MAX));
        let position_raw = i128::from(ctx.position(INSTRUMENT).raw());

        if target_qty_raw > position_raw {
            let requested_raw = (target_qty_raw - position_raw).min(i128::from(i64::MAX)) as i64;
            if requested_raw <= 0 {
                return (None, false);
            }
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if !affordable.is_positive() {
                return (None, true);
            }
            let qty_raw = requested_raw.min(affordable.raw());
            if qty_raw <= 0 {
                return (None, true);
            }
            (Some((Side::Buy, Qty::from_raw(qty_raw))), false)
        } else if position_raw > target_qty_raw {
            let sell_raw = (position_raw - target_qty_raw).min(i128::from(i64::MAX)) as i64;
            if sell_raw > 0 {
                (Some((Side::Sell, Qty::from_raw(sell_raw))), false)
            } else {
                (None, false)
            }
        } else {
            (None, false)
        }
    }

    fn submit(&mut self, ctx: &mut Ctx, side: Side, qty: Qty) {
        self.order_active = true;
        self.fill_notional = 0;
        self.fill_qty = 0;
        ctx.submit(
            OrderRequest::market(INSTRUMENT, side, qty)
                .with_time_in_force(TimeInForce::Gtc),
        );
    }

    fn finish_order(&mut self) {
        if self.fill_qty > 0 {
            let price_raw = self.fill_notional / self.fill_qty;
            if price_raw > 0 && price_raw <= i128::from(i64::MAX) {
                self.reference = Some(Price::from_raw(price_raw as i64));
            }
        }
        self.order_active = false;
        self.fill_notional = 0;
        self.fill_qty = 0;
    }

    fn is_hourly_close(bar: &Bar) -> bool {
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        if minute_ns <= 0 {
            return false;
        }
        let minute = bar.close_ts.as_nanos().div_euclid(minute_ns);
        minute.rem_euclid(60) == 0
    }

    fn crossed_threshold(close: Price, reference: Price) -> bool {
        let current = i128::from(close.raw());
        let reference = i128::from(reference.raw());
        if current <= 0 || reference <= 0 {
            return false;
        }
        current * 100 >= reference * 108 || current * 100 <= reference * 92
    }
}

impl Strategy for FixedWeightVolatilityHarvest {
    fn name(&self) -> String {
        "fixed-weight-volatility-harvest".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        if self.order_active && ctx.open_orders() == 0 {
            self.finish_order();
        }
        if self.order_active || ctx.open_orders() != 0 {
            return;
        }

        // Establish the initial 90% position before applying hourly triggers.
        if self.reference.is_none() {
            let (plan, blocked) = Self::target_order(ctx, bar.close);
            if let Some((side, qty)) = plan {
                self.submit(ctx, side, qty);
            } else if !blocked {
                self.reference = Some(bar.close);
            }
            return;
        }

        if !Self::is_hourly_close(bar) {
            return;
        }
        let reference = self.reference.expect("reference checked above");
        if !Self::crossed_threshold(bar.close, reference) {
            return;
        }

        let (plan, _) = Self::target_order(ctx, bar.close);
        if let Some((side, qty)) = plan {
            self.submit(ctx, side, qty);
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if !self.order_active || fill.qty.raw() <= 0 {
            return;
        }
        self.fill_notional = self
            .fill_notional
            .saturating_add(i128::from(fill.price.raw()).saturating_mul(i128::from(fill.qty.raw())));
        self.fill_qty = self.fill_qty.saturating_add(i128::from(fill.qty.raw()));
    }
}

export_strategy!(FixedWeightVolatilityHarvest {
    reference: None,
    order_active: false,
    fill_notional: 0,
    fill_qty: 0,
});
