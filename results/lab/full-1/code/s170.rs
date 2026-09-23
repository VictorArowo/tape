use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, TimeInForce,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const ENTRY_MINUTE: i64 = 9 * 60 + 34;
const EXIT_MINUTE: i64 = 15 * 60 + 54;

// Convert Gregorian civil dates to days since the Unix epoch.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let adjusted_year = year - if month <= 2 { 1 } else { 0 };
    let era = if adjusted_year >= 0 {
        adjusted_year / 400
    } else {
        (adjusted_year - 399) / 400
    };
    let year_of_era = adjusted_year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146097 + day_of_era - 719468
}

// Convert days since the Unix epoch to a Gregorian year, month, and day.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719468;
    let era = if z >= 0 { z / 146097 } else { (z - 146096) / 146097 };
    let day_of_era = z - era * 146097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36524
        - day_of_era / 146096)
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
    (year, month, day)
}

fn sunday_in_month(year: i64, month: i64, occurrence: i64) -> i64 {
    // Sunday is zero; the Unix epoch day (1970-01-01) was a Thursday.
    let first_day = days_from_civil(year, month, 1);
    let first_weekday = (first_day + 4).rem_euclid(7);
    let first_sunday = 1 + (7 - first_weekday).rem_euclid(7);
    first_sunday + (occurrence - 1) * 7
}

// US daylight saving time transitions, expressed in UTC:
// second Sunday in March at 07:00, through first Sunday in November at 06:00.
fn is_us_eastern_dst(timestamp_ns: i64) -> bool {
    let day_ns = TimeDelta::from_days(1).as_nanos();
    let utc_day = timestamp_ns.div_euclid(day_ns);
    let nanos_in_day = timestamp_ns.rem_euclid(day_ns);
    let (year, _, _) = civil_from_days(utc_day);

    let start_day = days_from_civil(year, 3, sunday_in_month(year, 3, 2));
    let end_day = days_from_civil(year, 11, sunday_in_month(year, 11, 1));
    let start = (start_day, TimeDelta::from_hours(7).as_nanos());
    let end = (end_day, TimeDelta::from_hours(6).as_nanos());
    (utc_day, nanos_in_day) >= start && (utc_day, nanos_in_day) < end
}

fn eastern_weekday_and_minute(timestamp_ns: i64) -> (i64, i64) {
    let minute_ns = TimeDelta::from_mins(1).as_nanos();
    let utc_minute = timestamp_ns.div_euclid(minute_ns);
    let offset_minutes = if is_us_eastern_dst(timestamp_ns) {
        -4 * 60
    } else {
        -5 * 60
    };
    let local_minute = utc_minute + offset_minutes;
    let local_day = local_minute.div_euclid(1440);
    let minute_of_day = local_minute.rem_euclid(1440);
    // SDK weekday convention: 0 = Thursday.
    let weekday = (local_day + 4).rem_euclid(7);
    (weekday, minute_of_day)
}

struct UsCashSessionExposure {
    exit_pending: bool,
}

impl Strategy for UsCashSessionExposure {
    fn name(&self) -> String {
        "us-cash-session-exposure".to_owned()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        // Keep retrying an exit on successive bars until the position is flat.
        if self.exit_pending {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                if ctx.open_orders() == 0 {
                    ctx.submit(
                        OrderRequest::market(INSTRUMENT, Side::Sell, position)
                            .with_time_in_force(TimeInForce::Ioc),
                    );
                }
                return;
            }
            self.exit_pending = false;
        }

        let (weekday, minute) = eastern_weekday_and_minute(bar.open_ts.as_nanos());
        let is_weekday = weekday >= 4 || weekday <= 1;
        if !is_weekday {
            return;
        }

        if minute == EXIT_MINUTE {
            self.exit_pending = true;
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() && ctx.open_orders() == 0 {
                ctx.submit(
                    OrderRequest::market(INSTRUMENT, Side::Sell, position)
                        .with_time_in_force(TimeInForce::Ioc),
                );
            }
            return;
        }

        if minute == ENTRY_MINUTE
            && ctx.position(INSTRUMENT).is_zero()
            && ctx.open_orders() == 0
        {
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                ctx.submit(
                    OrderRequest::market(INSTRUMENT, Side::Buy, qty)
                        .with_time_in_force(TimeInForce::Ioc),
                );
            }
        }
    }
}

export_strategy!(UsCashSessionExposure { exit_pending: false });
