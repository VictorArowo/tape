use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

struct WeekendLiquidityAvoidance;

impl Strategy for WeekendLiquidityAvoidance {
    fn name(&self) -> String {
        String::from("weekend-liquidity-avoidance")
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT || ctx.open_orders() != 0 {
            return;
        }

        let nanos = bar.close_ts.as_nanos();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let day_of_week = nanos.div_euclid(day_ns).rem_euclid(7); // Thursday = 0
        let minute_of_day = nanos.div_euclid(minute_ns).rem_euclid(1_440);
        let monday_based_day = (day_of_week + 3).rem_euclid(7);
        let minute_of_week = monday_based_day * 1_440 + minute_of_day;

        // Hold BTC from Monday 00:00 up to, but not including, Friday 20:00 UTC.
        let should_hold_btc = minute_of_week < 4 * 1_440 + 20 * 60;
        let position = ctx.position(INSTRUMENT);

        if should_hold_btc {
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        } else if position.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
        }
    }
}

export_strategy!(WeekendLiquidityAvoidance);