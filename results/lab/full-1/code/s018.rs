use tape_guest::{export_strategy, Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const REFERENCE_MINUTE: i64 = 870; // 14:30 UTC
const SIGNAL_MINUTE: i64 = 1260; // 21:00 UTC
const EXIT_MINUTE: i64 = 300; // 05:00 UTC
const SIZING_MARGIN_PPM: i64 = 5_000;

struct UsSessionOvernightDrift {
    reference_close: Option<(i64, i64)>, // (UTC day, raw close)
    exit_day: Option<i64>,
}

impl Strategy for UsSessionOvernightDrift {
    fn name(&self) -> String {
        "US-Session Overnight Drift".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let nanos = bar.close_ts.as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day = nanos / day_ns;
        let minute = (nanos / minute_ns) % 1440;

        if minute == REFERENCE_MINUTE {
            self.reference_close = Some((day, bar.close.raw()));
        }

        // Submit at the 05:00 boundary so the market order is eligible at
        // the next bar's open. Retry on later bars if the position remains.
        if let Some(exit_day) = self.exit_day {
            let exit_due = day > exit_day || (day == exit_day && minute >= EXIT_MINUTE);
            if exit_due {
                let position = ctx.position(INSTRUMENT);
                if position.is_positive() {
                    if ctx.open_orders() == 0 {
                        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                    }
                } else {
                    self.exit_day = None;
                }
            }
        }

        if minute != SIGNAL_MINUTE {
            return;
        }

        let reference = self.reference_close.take();
        let Some((reference_day, reference_raw)) = reference else {
            return;
        };
        if reference_day != day || reference_raw <= 0 {
            return;
        }

        // Strictly greater than a 0.75% session return.
        let return_is_large_enough = i128::from(bar.close.raw()) * 10_000
            > i128::from(reference_raw) * 10_075;
        if !return_is_large_enough
            || !ctx.position(INSTRUMENT).is_zero()
            || ctx.open_orders() != 0
            || self.exit_day.is_some()
            || !bar.close.is_positive()
        {
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
            self.exit_day = Some(day + 1);
        }
    }
}

export_strategy!(UsSessionOvernightDrift {
    reference_close: None,
    exit_day: None,
});
