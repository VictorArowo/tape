use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

struct UnfilteredSpotHold {
    target_qty: Option<i64>,
}

impl Strategy for UnfilteredSpotHold {
    fn name(&self) -> String {
        "Unfiltered Spot Hold".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        if self.target_qty.is_none() {
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            self.target_qty = Some(affordable.raw());
        }

        if ctx.open_orders() != 0 {
            return;
        }

        let remaining_raw = self.target_qty.unwrap_or(0) - ctx.position(INSTRUMENT).raw();
        if remaining_raw <= 0 {
            return;
        }

        // Limit each bar's attempted buy to one tenth of its reported volume.
        let volume_cap = bar.volume.mul_ratio(1, 10);
        let remaining = tape_guest::Qty::from_raw(remaining_raw);
        let qty = if remaining < volume_cap { remaining } else { volume_cap };
        if qty.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }
}

export_strategy!(UnfilteredSpotHold { target_qty: None });
