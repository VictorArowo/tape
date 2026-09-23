use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const MINUTES_PER_DAY: i64 = 1_440;

struct WeekendDriftContinuation {
    friday_day: Option<i64>,
    friday_midnight_close: Option<Price>,
}

impl WeekendDriftContinuation {
    fn utc_day_and_minute(ts: tape_guest::Timestamp) -> (i64, i64) {
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let total_minutes = ts.as_nanos().div_euclid(minute_ns);
        (
            total_minutes.div_euclid(MINUTES_PER_DAY),
            total_minutes.rem_euclid(MINUTES_PER_DAY),
        )
    }
}

impl Strategy for WeekendDriftContinuation {
    fn name(&self) -> String {
        "weekend-drift-continuation".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        // Use the bar's opening timestamp to identify the stated UTC intervals.
        let (utc_day, minute_of_day) = Self::utc_day_and_minute(bar.open_ts);
        let weekday = utc_day.rem_euclid(7); // 0 = Thursday, 1 = Friday, 3 = Sunday.

        if weekday == 1 {
            if self.friday_day != Some(utc_day) {
                self.friday_day = Some(utc_day);
                self.friday_midnight_close = None;
            }
            if minute_of_day == 0 {
                self.friday_midnight_close = Some(bar.close);
            }
        }

        if ctx.open_orders() != 0 {
            return;
        }

        // Friday 19:59–20:00: enter for execution at the next bar's open.
        if weekday == 1 && minute_of_day == 1_199 {
            if let Some(midnight_close) = self.friday_midnight_close {
                if bar.close > midnight_close && ctx.position(INSTRUMENT).is_zero() {
                    let affordable = ctx.affordable_qty(
                        INSTRUMENT,
                        bar.close,
                        ctx.cash(),
                        SIZING_MARGIN_PPM,
                    );
                    let volume_cap = bar.volume.mul_ratio(1, 10);
                    let qty = if affordable < volume_cap {
                        affordable
                    } else {
                        volume_cap
                    };
                    if qty.is_positive() {
                        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                    }
                }
            }
        }

        // Sunday 23:59–Monday 00:00: exit for execution at the next bar's open.
        if weekday == 3 && minute_of_day == 1_439 {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
        }
    }
}

export_strategy!(WeekendDriftContinuation {
    friday_day: None,
    friday_midnight_close: None,
});
