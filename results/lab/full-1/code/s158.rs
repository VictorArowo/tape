use tape_guest::{
    export_strategy, Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

fn civil_date(days_since_epoch: i64) -> (i64, i64, i64) {
    // Convert Unix-relative days to a Gregorian year, month, and day.
    let z = days_since_epoch + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524
        - day_of_era / 146_096)
        / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_part = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = month_part + if month_part < 10 { 3 } else { -9 };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

struct MonthTurnRebound {
    last_buy_month: Option<i64>,
    last_sell_day: Option<i64>,
}

impl Strategy for MonthTurnRebound {
    fn name(&self) -> String {
        "Month-Turn Rebound".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let timestamp = bar.close_ts.as_nanos();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let minute_of_day = timestamp.div_euclid(minute_ns).rem_euclid(1_440);
        let day_index = timestamp.div_euclid(day_ns);
        let (year, month, day) = civil_date(day_index);

        // The boundary is the close of the preceding bar; submitting here
        // lets the market order execute at the following bar's open.
        if ctx.open_orders() != 0 {
            return;
        }

        if minute_of_day == 0 && day == 1 {
            let month_key = year * 12 + month;
            if self.last_buy_month != Some(month_key) {
                self.last_buy_month = Some(month_key);
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
        } else if minute_of_day == 360 && self.last_sell_day != Some(day_index) {
            self.last_sell_day = Some(day_index);
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
        }
    }
}

export_strategy!(MonthTurnRebound {
    last_buy_month: None,
    last_sell_day: None,
});
