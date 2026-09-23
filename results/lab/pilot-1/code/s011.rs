use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

// Returns the Gregorian month and day for a count of days since 1970-01-01.
fn month_day(days_since_epoch: i64) -> (i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524
        - day_of_era / 146_096)
        / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era
        - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    if month <= 2 {
        year += 1;
    }
    let _ = year;
    (month, day)
}

fn is_month_end_day(days_since_epoch: i64) -> bool {
    let (month, day) = month_day(days_since_epoch);
    let next_month = month_day(days_since_epoch + 1).0;
    month != next_month
}

struct MonthEndFlowWindow;

impl Strategy for MonthEndFlowWindow {
    fn name(&self) -> String {
        "Month-End Flow Window".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT || ctx.open_orders() != 0 {
            return;
        }

        let day_nanos = TimeDelta::from_days(1).as_nanos();
        let minute_nanos = TimeDelta::from_mins(1).as_nanos();
        let timestamp = bar.close_ts.as_nanos();
        if timestamp.rem_euclid(day_nanos) != 0 {
            return;
        }

        let days = timestamp.div_euclid(day_nanos);
        let (_, day_of_month) = month_day(days);
        let position = ctx.position(INSTRUMENT);

        if is_month_end_day(days) && position.is_zero() {
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        } else if day_of_month == 2 && position.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
        }

        let _ = minute_nanos;
    }
}

export_strategy!(MonthEndFlowWindow);
