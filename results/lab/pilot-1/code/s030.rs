use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const MINUTES_PER_DAY: i64 = 1_440;
const WINDOW_END_MINUTE: i64 = 360;

struct UtcOvernightDrift;

impl Strategy for UtcOvernightDrift {
    fn name(&self) -> String {
        "UTC Overnight Drift".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT || ctx.open_orders() != 0 {
            return;
        }

        // The bar callback occurs at close. Its close timestamp is the next
        // bar's expected open, so orders submitted here execute at that open.
        let minute_of_day = (bar.close_ts.as_nanos() / TimeDelta::from_mins(1).as_nanos())
            .rem_euclid(MINUTES_PER_DAY);
        let position = ctx.position(INSTRUMENT);

        if minute_of_day < WINDOW_END_MINUTE && position.is_zero() {
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        } else if minute_of_day >= WINDOW_END_MINUTE && position.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
        }
    }
}

export_strategy!(UtcOvernightDrift);
