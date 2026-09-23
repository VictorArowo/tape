use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const THURSDAY: i64 = 0;
const FRIDAY: i64 = 1;
const SUNDAY: i64 = 3;
const LAST_MINUTE_OF_DAY: i64 = 1_439;

struct WeekendLiquidityGate;

impl Strategy for WeekendLiquidityGate {
    fn name(&self) -> String {
        "Weekend Liquidity Gate".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT || ctx.open_orders() != 0 {
            return;
        }

        // The timestamps are treated as bar opens. Weekday numbering is 0 = Thursday.
        let timestamp_ns = bar.open_ts.as_nanos();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let minute_of_day = timestamp_ns.rem_euclid(day_ns) / minute_ns;
        let day_of_week = timestamp_ns.div_euclid(day_ns).rem_euclid(7);

        if minute_of_day != LAST_MINUTE_OF_DAY {
            return;
        }

        if day_of_week == FRIDAY {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
        } else if day_of_week == SUNDAY {
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
}

export_strategy!(WeekendLiquidityGate);
