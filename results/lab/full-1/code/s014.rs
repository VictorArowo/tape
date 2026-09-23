use tape_guest::{
    Bar, Ctx, InstrumentId, OrderId, OrderRequest, Side, Strategy, TimeDelta, TimeInForce,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_BUFFER_PPM: i64 = 1_200; // 0.10% fee + 0.02% slippage
const FINAL_MINUTE: i64 = 1_439;
const SUNDAY: i64 = 3; // Day-of-week numbering uses Thursday = 0.
const THURSDAY: i64 = 0;

struct WeekdayOnlyBtc {
    pending_order: Option<OrderId>,
}

fn is_final_minute_of_day(ts: tape_guest::Timestamp, day: i64) -> bool {
    let nanos = ts.as_nanos();
    let minute_ns = TimeDelta::from_mins(1).as_nanos();
    let day_ns = TimeDelta::from_days(1).as_nanos();
    let minute_of_day = nanos.div_euclid(minute_ns).rem_euclid(1_440);
    let day_of_week = nanos.div_euclid(day_ns).rem_euclid(7);
    minute_of_day == FINAL_MINUTE && day_of_week == day
}

impl Strategy for WeekdayOnlyBtc {
    fn name(&self) -> String {
        "weekday-only-btc".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        if ctx.open_orders() == 0 {
            self.pending_order = None;
        }

        if is_final_minute_of_day(bar.close_ts, THURSDAY) {
            // Cancel any remaining entry quantity before placing the scheduled exit.
            if let Some(order_id) = self.pending_order.take() {
                ctx.cancel(order_id);
            } else if ctx.open_orders() != 0 {
                return;
            }

            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                let id = ctx.submit(
                    OrderRequest::market(INSTRUMENT, Side::Sell, position)
                        .with_time_in_force(TimeInForce::Gtc),
                );
                self.pending_order = Some(id);
            }
            return;
        }

        if !is_final_minute_of_day(bar.close_ts, SUNDAY) {
            return;
        }
        if ctx.open_orders() != 0 || !ctx.position(INSTRUMENT).is_zero() {
            return;
        }

        let qty = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            ctx.cash(),
            SIZING_BUFFER_PPM,
        );
        if qty.is_positive() {
            let id = ctx.submit(
                OrderRequest::market(INSTRUMENT, Side::Buy, qty)
                    .with_time_in_force(TimeInForce::Gtc),
            );
            self.pending_order = Some(id);
        }
    }
}

export_strategy!(WeekdayOnlyBtc {
    pending_order: None,
});
