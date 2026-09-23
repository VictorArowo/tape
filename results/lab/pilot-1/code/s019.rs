use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

struct DrawdownBudgetExit {
    peak: Option<i64>,
    trough: Option<i64>,
    exited: bool,
}

impl Strategy for DrawdownBudgetExit {
    fn name(&self) -> String {
        "Drawdown Budget Exit".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let position = ctx.position(INSTRUMENT);

        // Let any outstanding order finish before evaluating another signal.
        if ctx.open_orders() != 0 {
            return;
        }

        if position.is_positive() {
            let peak_raw = *self.peak.get_or_insert(bar.close.raw());
            let peak = tape_guest::Price::from_raw(peak_raw);
            if bar.close <= peak.mul_ratio(3, 4) {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                self.exited = true;
                self.trough = None;
            } else if bar.close.raw() > peak_raw {
                self.peak = Some(bar.close.raw());
            }
            return;
        }

        if !self.exited {
            // Initial investment: size using available cash and the completed close.
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                self.peak = None;
            }
            return;
        }

        let trough_raw = *self.trough.get_or_insert(bar.close.raw());
        let trough = tape_guest::Price::from_raw(trough_raw);
        if bar.close >= trough.mul_ratio(11, 10) {
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                self.exited = false;
                self.peak = None;
                self.trough = None;
            }
        } else if bar.close.raw() < trough_raw {
            self.trough = Some(bar.close.raw());
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.side == Side::Buy {
            self.peak = None;
        } else if fill.side == Side::Sell && ctx.position(INSTRUMENT).is_zero() {
            self.exited = true;
            self.trough = None;
        }
    }
}

export_strategy!(DrawdownBudgetExit {
    peak: None,
    trough: None,
    exited: false,
});
