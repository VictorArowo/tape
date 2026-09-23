use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const REFERENCE_CLOSE_MINUTE: i64 = 13 * 60; // Close of the 12:59 UTC bar
const ENTRY_MINUTE: i64 = 15 * 60;
const EXIT_MINUTE: i64 = 19 * 60;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct UsWindowMomentum {
    reference_day: Option<i64>,
    reference_close_raw: Option<i64>,
    evaluated_day: Option<i64>,
}

impl Strategy for UsWindowMomentum {
    fn name(&self) -> String {
        "US-Window Momentum".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        // Minute bars are timestamped at their close boundary: 13:00 is the
        // close of the 12:59 bar, and 15:00/19:00 are the target bar opens.
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let close_ns = bar.close_ts.as_nanos();
        let minute = close_ns.div_euclid(minute_ns).rem_euclid(1440);
        let day = close_ns.div_euclid(day_ns);

        if minute == REFERENCE_CLOSE_MINUTE {
            self.reference_day = Some(day);
            self.reference_close_raw = Some(bar.close.raw());
        }

        if minute == EXIT_MINUTE {
            if ctx.open_orders() == 0 {
                let position = ctx.position(INSTRUMENT);
                if position.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                }
            }
            return;
        }

        if minute != ENTRY_MINUTE || self.evaluated_day == Some(day) {
            return;
        }
        self.evaluated_day = Some(day);

        let (Some(reference_day), Some(reference_raw)) =
            (self.reference_day, self.reference_close_raw)
        else {
            return;
        };
        if reference_day != day || reference_raw <= 0 || bar.close.raw() <= 0 {
            return;
        }

        if ctx.open_orders() != 0 || !ctx.position(INSTRUMENT).is_zero() {
            return;
        }

        // Exact integer comparison for a return of at least 0.5%.
        if i128::from(bar.close.raw()) * 1_000 < i128::from(reference_raw) * 1_005 {
            return;
        }

        let qty = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        if qty.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }
}

export_strategy!(UsWindowMomentum {
    reference_day: None,
    reference_close_raw: None,
    evaluated_day: None,
});
