use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

struct FridayExpiryWindow;

impl Strategy for FridayExpiryWindow {
    fn name(&self) -> String {
        "Friday Expiry Window".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT || ctx.open_orders() != 0 {
            return;
        }

        let timestamp = bar.close_ts.as_nanos();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let minute_of_day = (timestamp / minute_ns).rem_euclid(1440);
        let day_of_week = (timestamp / day_ns).rem_euclid(7); // 0 = Thursday; 1 = Friday

        if day_of_week != 1 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if minute_of_day == 480 && position.is_zero() {
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        } else if minute_of_day == 960 && position.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
        }
    }
}

export_strategy!(FridayExpiryWindow);
