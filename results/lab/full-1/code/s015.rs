use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    WaitingForMonday,
    Long,
    WeekendFlat,
}

struct WeekendLiquidityAvoidance {
    phase: Phase,
}

impl Strategy for WeekendLiquidityAvoidance {
    fn name(&self) -> String {
        "weekend-liquidity-avoidance".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        // A bar closes at the next bar's open timestamp, so schedule orders
        // against bar.close_ts to target the Monday/Saturday boundary open.
        let ts = bar.close_ts.as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day = ts.div_euclid(day_ns);
        let minute_of_day = ts.rem_euclid(day_ns) / minute_ns;
        let day_of_week = day.rem_euclid(7); // Thursday = 0; Saturday = 2, Monday = 4.

        if minute_of_day == 0 && day_of_week == 4 {
            self.phase = Phase::Long;
        } else if day_of_week == 2 || day_of_week == 3 {
            self.phase = Phase::WeekendFlat;
        } else if self.phase == Phase::WeekendFlat {
            // Do not enter midweek if the strategy started during the weekend
            // or the Saturday liquidation window has ended.
            self.phase = Phase::WaitingForMonday;
        }

        if ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        match self.phase {
            Phase::Long => {
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
            Phase::WeekendFlat => {
                if position.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                }
            }
            Phase::WaitingForMonday => {}
        }
    }

    fn on_finish(&mut self, ctx: &mut Ctx) {
        // The SDK does not expose advance notice of the final bar. Submit a
        // final liquidation request when possible as a best-effort fallback.
        let position = ctx.position(INSTRUMENT);
        if ctx.open_orders() == 0 && position.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
        }
    }
}

export_strategy!(WeekendLiquidityAvoidance {
    phase: Phase::WaitingForMonday,
});
