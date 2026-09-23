use tape_guest::{Bar, Ctx, InstrumentId, Money, OrderRequest, Price, Qty, Side, Strategy, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const RAW_SCALE: i128 = 100_000_000;

struct BtcBandRebalancing {
    saw_first_bar: bool,
}

impl BtcBandRebalancing {
    fn target_qty(equity: Money, price: Price) -> Qty {
        if !equity.is_positive() || !price.is_positive() {
            return Qty::from_raw(0);
        }

        let target_value_raw = i128::from(equity.raw()) * 95 / 100;
        let qty_raw = target_value_raw * RAW_SCALE / i128::from(price.raw());
        Qty::from_raw(qty_raw.clamp(0, i128::from(i64::MAX)) as i64)
    }

    fn rebalance_to_target(&self, ctx: &mut Ctx, price: Price) {
        let position = ctx.position(INSTRUMENT);
        let equity = ctx.cash() + price * position;
        let target = Self::target_qty(equity, price);

        if target.raw() > position.raw() {
            let needed_raw = target.raw() - position.raw();
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                price,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            let qty_raw = needed_raw.min(affordable.raw()).max(0);
            if qty_raw > 0 {
                ctx.submit(OrderRequest::market(
                    INSTRUMENT,
                    Side::Buy,
                    Qty::from_raw(qty_raw),
                ));
            }
        } else if position.raw() > target.raw() {
            let qty_raw = (position.raw() - target.raw()).min(position.raw());
            if qty_raw > 0 {
                ctx.submit(OrderRequest::market(
                    INSTRUMENT,
                    Side::Sell,
                    Qty::from_raw(qty_raw),
                ));
            }
        }
    }
}

impl Strategy for BtcBandRebalancing {
    fn name(&self) -> String {
        "BTC Band Rebalancing".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT || ctx.open_orders() != 0 {
            return;
        }

        if !self.saw_first_bar {
            self.saw_first_bar = true;
            self.rebalance_to_target(ctx, bar.close);
            return;
        }

        let position = ctx.position(INSTRUMENT);
        let btc_value = bar.close * position;
        let equity = ctx.cash() + btc_value;
        if !equity.is_positive() {
            return;
        }

        if btc_value >= equity || btc_value <= equity.mul_ratio(90, 100) {
            self.rebalance_to_target(ctx, bar.close);
        }
    }
}

export_strategy!(BtcBandRebalancing { saw_first_bar: false });
