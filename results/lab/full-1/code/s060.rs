use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

struct WeekendLiquidityAvoidance;

impl Strategy for WeekendLiquidityAvoidance {
    fn name(&self) -> String {
        "weekend-liquidity-avoidance".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT || ctx.open_orders() != 0 {
            return;
        }

        let minute_nanos = TimeDelta::from_mins(1).as_nanos();
        let day_nanos = TimeDelta::from_days(1).as_nanos();
        let minute_of_day = (bar.close_ts.as_nanos() / minute_nanos) % 1440;
        let day_of_week = (bar.close_ts.as_nanos() / day_nanos) % 7;

        if minute_of_day != 1439 {
            return;
        }

        // Day 0 is Thursday: Friday is 1 and Sunday is 3.
        if day_of_week == 3 && ctx.position(INSTRUMENT).is_zero() {
            let budget = ctx.cash().mul_ratio(995, 1000);
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                budget,
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        } else if day_of_week == 1 {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
        }
    }
}

export_strategy!(WeekendLiquidityAvoidance);