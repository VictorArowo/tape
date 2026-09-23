use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const MAX_PARTICIPATION_NUM: i64 = 1;
const MAX_PARTICIPATION_DEN: i64 = 10;
const SIZING_MARGIN_PPM: i64 = 5_000;
const FRIDAY: i64 = 1; // Weekday numbering is 0 = Thursday.
const SUNDAY: i64 = 3;
const LAST_MINUTE_OF_DAY: i64 = 1_439;

struct WeekendLiquidityAvoidance {
    hold_long: bool,
}

impl Strategy for WeekendLiquidityAvoidance {
    fn name(&self) -> String {
        "weekend-liquidity-avoidance".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let timestamp = bar.close_ts.as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let weekday = timestamp.div_euclid(day_ns).rem_euclid(7);
        let minute_of_day = timestamp.rem_euclid(day_ns) / minute_ns;

        if minute_of_day == LAST_MINUTE_OF_DAY {
            if weekday == FRIDAY {
                self.hold_long = false;
            } else if weekday == SUNDAY {
                self.hold_long = true;
            }
        }

        // Market orders are IOC; a partial fill is handled by submitting the
        // remaining exposure again on a later bar.
        if ctx.open_orders() != 0 {
            return;
        }

        let bar_limit = bar
            .volume
            .mul_ratio(MAX_PARTICIPATION_NUM, MAX_PARTICIPATION_DEN);
        if !bar_limit.is_positive() {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if self.hold_long {
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            let qty = Qty::from_raw(affordable.raw().min(bar_limit.raw()));
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        } else if position.is_positive() {
            let qty = Qty::from_raw(position.raw().min(bar_limit.raw()));
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
            }
        }
    }
}

export_strategy!(WeekendLiquidityAvoidance { hold_long: false });
