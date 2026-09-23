use tape_guest::{
    export_strategy, Bar, Ctx, Fill, InstrumentId, OrderRequest, Side, Strategy, TimeDelta,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

struct PostExpiryContinuation {
    friday_midnight_close: Option<(i64, i64)>, // (day index, raw close)
    last_evaluated_friday: Option<i64>,
    exit_at_ns: Option<i64>,
}

impl Strategy for PostExpiryContinuation {
    fn name(&self) -> String {
        "post-expiry-continuation".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let ts_ns = bar.close_ts.as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_index = ts_ns / day_ns;
        let minute_of_day = (ts_ns / minute_ns) % 1440;
        let is_friday = day_index % 7 == 1; // 0 = Thursday

        if is_friday && minute_of_day == 0 {
            self.friday_midnight_close = Some((day_index, bar.close.raw()));
        }

        // Exit at the first available bar open at or after 12 hours from entry.
        if let Some(exit_at_ns) = self.exit_at_ns {
            if ts_ns >= exit_at_ns {
                let position = ctx.position(INSTRUMENT);
                if position.is_zero() && ctx.open_orders() == 0 {
                    self.exit_at_ns = None;
                } else if position.is_positive() && ctx.open_orders() == 0 {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                }
            }
            // Do not consider a new entry while a trade is active or being exited.
            if self.exit_at_ns.is_some() {
                return;
            }
        }

        if !is_friday || minute_of_day != 480 {
            return;
        }

        // Evaluate only once per Friday, using the completed 00:00 and 08:00 closes.
        if self.last_evaluated_friday == Some(day_index) {
            return;
        }
        self.last_evaluated_friday = Some(day_index);

        let Some((reference_day, reference_close_raw)) = self.friday_midnight_close else {
            return;
        };
        if reference_day != day_index {
            return;
        }

        // close_08 / close_00 > 1.005, compared exactly using integers.
        let continuation = i128::from(bar.close.raw()) * 200
            > i128::from(reference_close_raw) * 201;
        if !continuation || !ctx.position(INSTRUMENT).is_zero() || ctx.open_orders() != 0 {
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

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Buy && self.exit_at_ns.is_none() {
            self.exit_at_ns = Some(
                fill.ts.as_nanos() + TimeDelta::from_hours(12).as_nanos(),
            );
        }
    }
}

export_strategy!(PostExpiryContinuation {
    friday_midnight_close: None,
    last_evaluated_friday: None,
    exit_at_ns: None,
});
