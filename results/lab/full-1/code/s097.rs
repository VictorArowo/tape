use tape_guest::{
    export_strategy, Bar, Ctx, InstrumentId, Money, OrderRequest, Price, Qty, Side, Strategy,
    TimeDelta,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SCALE: i128 = 100_000_000;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct BandVolatilityHarvest {
    initialized: bool,
    rebalance_active: bool,
    hourly_volume_raw: i128,
}

impl BandVolatilityHarvest {
    fn target_qty(equity: Money, price: Price) -> Option<Qty> {
        if equity.raw() <= 0 || price.raw() <= 0 {
            return None;
        }
        let target_value_raw = i128::from(equity.raw()) * 80 / 100;
        let qty_raw = target_value_raw * SCALE / i128::from(price.raw());
        Some(Qty::from_raw(qty_raw.clamp(0, i128::from(i64::MAX)) as i64))
    }

    fn cap_qty(raw: i128) -> Qty {
        Qty::from_raw(raw.clamp(0, i128::from(i64::MAX)) as i64)
    }

    fn submit_toward_target(&mut self, ctx: &mut Ctx, price: Price, volume_cap: Qty) {
        if ctx.open_orders() != 0 || !price.is_positive() || !volume_cap.is_positive() {
            return;
        }

        let Some(target) = Self::target_qty(ctx.equity(), price) else {
            return;
        };
        let held_raw = ctx.position(INSTRUMENT).raw().max(0);
        let target_raw = target.raw().max(0);

        if target_raw > held_raw {
            let gap = Self::cap_qty(i128::from(target_raw) - i128::from(held_raw));
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                price,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            let mut qty_raw = gap.raw().min(affordable.raw()).min(volume_cap.raw());
            if qty_raw > 0 {
                ctx.submit(OrderRequest::market(
                    INSTRUMENT,
                    Side::Buy,
                    Qty::from_raw(qty_raw),
                ));
            }
        } else if held_raw > target_raw {
            let gap = held_raw - target_raw;
            let qty_raw = gap.min(held_raw).min(volume_cap.raw());
            if qty_raw > 0 {
                ctx.submit(OrderRequest::market(
                    INSTRUMENT,
                    Side::Sell,
                    Qty::from_raw(qty_raw),
                ));
            }
        } else {
            self.rebalance_active = false;
        }
    }

    fn outside_band(ctx: &Ctx, price: Price) -> bool {
        let equity_raw = i128::from(ctx.equity().raw());
        if equity_raw <= 0 {
            return false;
        }
        let value_raw = i128::from((price * ctx.position(INSTRUMENT)).raw());
        value_raw * 100 < equity_raw * 75 || value_raw * 100 > equity_raw * 85
    }

    fn on_fill_recompute_target(&mut self, ctx: &Ctx, fill_price: Price) {
        if !self.rebalance_active {
            return;
        }
        let Some(target) = Self::target_qty(ctx.equity(), fill_price) else {
            return;
        };
        self.rebalance_active = ctx.position(INSTRUMENT).raw().max(0) != target.raw();
    }
}

impl Strategy for BandVolatilityHarvest {
    fn name(&self) -> String {
        "band-based-volatility-harvest [wasm]".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        self.hourly_volume_raw = self
            .hourly_volume_raw
            .saturating_add(i128::from(bar.volume.raw()).max(0));

        let minute = (bar.close_ts.as_nanos() / TimeDelta::from_mins(1).as_nanos()) % 1_440;
        let hourly_close = minute % 60 == 0;

        if !self.initialized {
            self.initialized = true;
            self.rebalance_active = true;
            let cap = Self::cap_qty(self.hourly_volume_raw / 10);
            self.submit_toward_target(ctx, bar.open, cap);
            if hourly_close {
                self.hourly_volume_raw = 0;
            }
            return;
        }

        if !hourly_close {
            return;
        }

        let volume_cap = Self::cap_qty(self.hourly_volume_raw / 10);
        self.hourly_volume_raw = 0;

        if !self.rebalance_active && Self::outside_band(ctx, bar.close) {
            self.rebalance_active = true;
        }
        if self.rebalance_active {
            self.submit_toward_target(ctx, bar.close, volume_cap);
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        self.on_fill_recompute_target(ctx, fill.price);
    }
}

export_strategy!(BandVolatilityHarvest {
    initialized: false,
    rebalance_active: false,
    hourly_volume_raw: 0,
});