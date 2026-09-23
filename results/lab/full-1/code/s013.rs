use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, TimeInForce,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

struct WeekdayExposure;

impl Strategy for WeekdayExposure {
    fn name(&self) -> String {
        "Weekday Exposure".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT || ctx.open_orders() != 0 {
            return;
        }

        let minute = (bar.close_ts.as_nanos() / TimeDelta::from_mins(1).as_nanos()) % 1_440;
        let day = (bar.close_ts.as_nanos() / TimeDelta::from_days(1).as_nanos()) % 7;
        if minute != 1_439 {
            return;
        }

        // With 0 = Thursday, Sunday is 3 and Friday is 1.
        if day == 3 && ctx.position(INSTRUMENT).is_zero() {
            let budget = ctx.cash().mul_ratio(998, 1_000);
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                budget,
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                ctx.submit(
                    OrderRequest::market(INSTRUMENT, Side::Buy, qty)
                        .with_time_in_force(TimeInForce::Ioc),
                );
            }
        } else if day == 1 {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                ctx.submit(
                    OrderRequest::market(INSTRUMENT, Side::Sell, position)
                        .with_time_in_force(TimeInForce::Ioc),
                );
            }
        }
    }
}

export_strategy!(WeekdayExposure);