use tape_guest::{export_strategy, Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const ENTRY_MINUTE: i64 = 9 * 60 + 30;
const EXIT_MINUTE: i64 = 16 * 60;
const SIZING_MARGIN_PPM: i64 = 5_000;
const MINUTES_PER_DAY: i64 = 24 * 60;

struct SessionFlowCarry {
    prior_signal: Option<bool>,
    session_day: Option<i64>,
    session_open_raw: Option<i64>,
}

impl SessionFlowCarry {
    fn new() -> Self {
        Self {
            prior_signal: None,
            session_day: None,
            session_open_raw: None,
        }
    }
}

impl Strategy for SessionFlowCarry {
    fn name(&self) -> String {
        "US Session Flow Carry".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let open_utc_minute = bar.open_ts.as_nanos().div_euclid(minute_ns);
        let close_utc_minute = bar.close_ts.as_nanos().div_euclid(minute_ns);
        let (open_day, open_minute) = new_york_time(open_utc_minute);
        let (close_day, close_minute) = new_york_time(close_utc_minute);

        if self.session_day != Some(open_day) {
            self.session_day = Some(open_day);
            self.session_open_raw = None;
        }

        // The callback for the 15:59 bar occurs at 16:00 ET, so a market
        // order submitted here targets the 16:00 bar open.
        if close_minute == EXIT_MINUTE && is_trading_day(close_day) {
            let position = ctx.position(INSTRUMENT);
            let volume_cap = bar.volume.mul_ratio(1, 10);
            let qty = Qty::from_raw(position.raw().min(volume_cap.raw()));
            if position.is_positive() && qty.is_positive() && ctx.open_orders() == 0 {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
            }
        }

        // The 09:30 bar callback arrives after that bar closes; the market
        // order therefore targets the next bar open (09:31 ET).
        if open_minute == ENTRY_MINUTE && is_trading_day(open_day) {
            let signal = self.prior_signal.take();
            if signal == Some(true)
                && ctx.position(INSTRUMENT).is_zero()
                && ctx.open_orders() == 0
                && bar.close.is_positive()
            {
                let affordable = ctx.affordable_qty(
                    INSTRUMENT,
                    bar.close,
                    ctx.cash(),
                    SIZING_MARGIN_PPM,
                );
                let volume_cap = bar.volume.mul_ratio(1, 10);
                let qty = Qty::from_raw(affordable.raw().min(volume_cap.raw()));
                if qty.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                }
            }
        }

        // The 16:00 bar's open is the endpoint of the prior-session return.
        if open_minute == EXIT_MINUTE && is_trading_day(open_day) {
            let signal = self.session_open_raw.map(|session_open| {
                i128::from(bar.open.raw()) * 200 > i128::from(session_open) * 201
            });
            self.prior_signal = signal;
        }

        if open_minute == ENTRY_MINUTE && is_trading_day(open_day) {
            self.session_open_raw = Some(bar.open.raw());
        }
    }
}

fn new_york_time(utc_minute: i64) -> (i64, i64) {
    let offset = if is_new_york_dst(utc_minute) {
        -4 * 60
    } else {
        -5 * 60
    };
    let local_minute = utc_minute + offset;
    (
        local_minute.div_euclid(MINUTES_PER_DAY),
        local_minute.rem_euclid(MINUTES_PER_DAY),
    )
}

fn is_new_york_dst(utc_minute: i64) -> bool {
    let utc_day = utc_minute.div_euclid(MINUTES_PER_DAY);
    let (year, _, _) = civil_from_days(utc_day);

    let (start_month, start_day) = if year == 1974 {
        (1, nth_weekday_day(year, 1, 0, 1))
    } else if year == 1975 {
        (2, nth_weekday_day(year, 2, 0, 4))
    } else if year >= 2007 {
        (3, nth_weekday_day(year, 3, 0, 2))
    } else if year >= 1987 {
        (4, nth_weekday_day(year, 4, 0, 1))
    } else if year >= 1967 {
        (4, last_weekday_day(year, 4, 0))
    } else {
        return false;
    };

    let end_month = if year >= 2007 { 11 } else { 10 };
    let end_day = if year >= 2007 {
        nth_weekday_day(year, 11, 0, 1)
    } else {
        last_weekday_day(year, 10, 0)
    };

    // The spring transition is 02:00 standard time (07:00 UTC); the fall
    // transition is 02:00 daylight time (06:00 UTC).
    let start = days_from_civil(year, start_month, start_day) * MINUTES_PER_DAY + 7 * 60;
    let end = days_from_civil(year, end_month, end_day) * MINUTES_PER_DAY + 6 * 60;
    utc_minute >= start && utc_minute < end
}

fn is_trading_day(day_index: i64) -> bool {
    let weekday = (day_index + 4).rem_euclid(7); // 0 = Thursday
    if !matches!(weekday, 0 | 1 | 4 | 5 | 6) {
        return false;
    }
    let (year, month, day) = civil_from_days(day_index);
    !is_us_market_holiday(year, month, day)
}

fn is_us_market_holiday(year: i64, month: i64, day: i64) -> bool {
    let date = days_from_civil(year, month, day);

    // New Year's Day is not observed on the preceding Friday when it falls
    // on Saturday; a Sunday holiday is observed on Monday.
    let jan1 = days_from_civil(year, 1, 1);
    let jan1_weekday = (jan1 + 4).rem_euclid(7); // Sunday = 0
    let new_year = if jan1_weekday == 0 {
        Some(jan1 + 1)
    } else if jan1_weekday == 6 {
        None
    } else {
        Some(jan1)
    };
    if new_year == Some(date) {
        return true;
    }

    let next_jan1 = days_from_civil(year + 1, 1, 1);
    let next_jan1_weekday = (next_jan1 + 4).rem_euclid(7);
    if next_jan1_weekday == 0 && date == next_jan1 + 1 {
        return true;
    }

    if date == observed_fixed_holiday(year, 7, 4)
        || date == observed_fixed_holiday(year, 12, 25)
    {
        return true;
    }
    if year >= 2022 && date == observed_fixed_holiday(year, 6, 19) {
        return true;
    }

    // Martin Luther King Jr. Day (since 1998), and Washington's Birthday.
    if year >= 1998 && date == nth_weekday_day(year, 1, 1, 3).pipe_month(year, 1) {
        return true;
    }
    if year >= 1971 && date == nth_weekday_day(year, 2, 1, 3).pipe_month(year, 2) {
        return true;
    }

    // Good Friday, Memorial Day, Labor Day, and Thanksgiving.
    if date == easter_sunday_day(year) - 2
        || date == last_weekday_day(year, 5, 1)
        || date == nth_weekday_day(year, 9, 1, 1)
        || date == nth_weekday_day(year, 11, 4, 4)
    {
        return true;
    }
    false
}

// Attach the year/month to the day-of-month helpers above.
trait MonthDay {
    fn pipe_month(self, year: i64, month: i64) -> i64;
}
impl MonthDay for i64 {
    fn pipe_month(self, year: i64, month: i64) -> i64 {
        days_from_civil(year, month, self)
    }
}

fn observed_fixed_holiday(year: i64, month: i64, day: i64) -> i64 {
    let date = days_from_civil(year, month, day);
    match (date + 4).rem_euclid(7) {
        6 => date - 1, // Saturday
        0 => date + 1, // Sunday
        _ => date,
    }
}

fn nth_weekday_day(year: i64, month: i64, weekday: i64, occurrence: i64) -> i64 {
    let first = days_from_civil(year, month, 1);
    let first_weekday = (first + 4).rem_euclid(7); // Sunday = 0
    1 + (weekday - first_weekday).rem_euclid(7) + 7 * (occurrence - 1)
}

fn last_weekday_day(year: i64, month: i64, weekday: i64) -> i64 {
    let last_day = days_in_month(year, month);
    let last = days_from_civil(year, month, last_day);
    last_day - ((last + 4).rem_euclid(7) - weekday).rem_euclid(7)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap_year(year) => 29,
        _ => 28,
    }
}

fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn easter_sunday_day(year: i64) -> i64 {
    let a = year % 19;
    let b = year / 100;
    let c = year % 100;
    let d = b / 4;
    let e = b % 4;
    let f = (b + 8) / 25;
    let g = (b - f + 1) / 3;
    let h = (19 * a + b - d - g + 15) % 30;
    let i = c / 4;
    let k = c % 4;
    let l = (32 + 2 * e + 2 * i - h - k) % 7;
    let m = (a + 11 * h + 22 * l) / 451;
    let month = (h + l - 7 * m + 114) / 31;
    let day = (h + l - 7 * m + 114) % 31 + 1;
    days_from_civil(year, month, day)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let adjusted_year = year - i64::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146097 + day_of_era - 719468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146096
    } / 146097;
    let day_of_era = shifted - era * 146097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36524
        - day_of_era / 146096)
        / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

export_strategy!(SessionFlowCarry::new());