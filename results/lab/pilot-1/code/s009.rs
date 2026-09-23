use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

fn days_in_year(y: i64) -> i64 {
    if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) { 366 } else { 365 }
}

// Gregorian civil date to days since 1970-01-01.
fn civil_day(y: i64, m: i64, d: i64) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = m + if m > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_day(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    y += i64::from(m <= 2);
    (y, m, d)
}

// Sunday=0 through Saturday=6.
fn weekday(day: i64) -> i64 { (day + 4).rem_euclid(7) }

fn nth_weekday(year: i64, month: i64, weekday_wanted: i64, n: i64) -> i64 {
    let first = civil_day(year, month, 1);
    1 + (weekday_wanted - weekday(first)).rem_euclid(7) + 7 * (n - 1)
}

fn last_weekday(year: i64, month: i64, weekday_wanted: i64, last_date: i64) -> i64 {
    let last = civil_day(year, month, last_date);
    last_date - (weekday(last) - weekday_wanted).rem_euclid(7)
}

fn easter_day(year: i64) -> i64 {
    // Gregorian computus.
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
    let date = (h + l - 7 * m + 114) % 31 + 1;
    civil_day(year, month, date)
}

fn observed_fixed(year: i64, month: i64, date: i64) -> i64 {
    let day = civil_day(year, month, date);
    match weekday(day) {
        6 => day - 1,
        0 => day + 1,
        _ => day,
    }
}

fn is_holiday(day: i64) -> bool {
    let (year, month, date) = civil_from_day(day);
    let fixed = |y: i64, m: i64, d: i64| observed_fixed(y, m, d) == day;
    // New Year's observed date may fall in the adjacent calendar year.
    if fixed(year - 1, 1, 1) || fixed(year, 1, 1) || fixed(year + 1, 1, 1) {
        return true;
    }
    if fixed(year, 7, 4) || fixed(year, 12, 25) || fixed(year, 6, 19) {
        return true;
    }
    if month == 1 && date == nth_weekday(year, 1, 1, 3) { return true; } // MLK Day
    if month == 2 && date == nth_weekday(year, 2, 1, 3) { return true; } // Washington's Birthday
    if month == 5 && date == last_weekday(year, 5, 1, 31) { return true; } // Memorial Day
    if month == 9 && date == nth_weekday(year, 9, 1, 1) { return true; } // Labor Day
    if month == 11 && date == nth_weekday(year, 11, 4, 4) { return true; } // Thanksgiving
    if year >= 2022 && month == 6 && observed_fixed(year, 6, 19) == day { return true; }
    let good_friday = easter_day(year) - 2;
    day == good_friday
}

fn is_trading_day(day: i64) -> bool {
    let wd = weekday(day);
    wd != 0 && wd != 6 && !is_holiday(day)
}

fn next_trading_day(day: i64) -> i64 {
    let mut candidate = day + 1;
    while !is_trading_day(candidate) { candidate += 1; }
    candidate
}

fn dst_start_utc_day(year: i64) -> i64 {
    civil_day(year, 3, nth_weekday(year, 3, 0, 2))
}

fn dst_end_utc_day(year: i64) -> i64 {
    civil_day(year, 11, nth_weekday(year, 11, 0, 1))
}

fn new_york_minute(utc_day: i64, utc_minute: i64) -> (i64, i64) {
    let (year, _, _) = civil_from_day(utc_day);
    let start_day = dst_start_utc_day(year);
    let end_day = dst_end_utc_day(year);
    // US daylight time begins at 07:00 UTC and ends at 06:00 UTC.
    let dst = (utc_day > start_day || (utc_day == start_day && utc_minute >= 420))
        && (utc_day < end_day || (utc_day == end_day && utc_minute < 360));
    let local_total = utc_day * 1440 + utc_minute - if dst { 240 } else { 300 };
    (local_total.div_euclid(1440), local_total.rem_euclid(1440))
}

struct SessionFlowReversal {
    exit_day: Option<i64>,
    exit_submitted: bool,
}

impl Strategy for SessionFlowReversal {
    fn name(&self) -> String { "U.S. Session Flow Reversal".to_string() }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT { return; }
        let nanos_per_day = TimeDelta::from_days(1).as_nanos();
        let nanos_per_minute = TimeDelta::from_mins(1).as_nanos();
        let utc_nanos = bar.close_ts.as_nanos();
        let utc_day = utc_nanos.div_euclid(nanos_per_day);
        let utc_minute = utc_nanos.rem_euclid(nanos_per_day) / nanos_per_minute;
        let (local_day, minute) = new_york_minute(utc_day, utc_minute);

        if self.exit_day == Some(local_day) && minute == 570 && !self.exit_submitted {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() && ctx.open_orders() == 0 {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                self.exit_submitted = true;
            }
            return;
        }

        if minute == 960 && self.exit_day.is_none() && ctx.position(INSTRUMENT).is_zero()
            && ctx.open_orders() == 0
        {
            let planned_exit = next_trading_day(local_day);
            // Only enter when the scheduled exit is the next equity trading session.
            if planned_exit > local_day {
                let qty = ctx.affordable_qty(INSTRUMENT, bar.close, ctx.cash(), SIZING_MARGIN_PPM);
                if qty.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                    self.exit_day = Some(planned_exit);
                    self.exit_submitted = false;
                }
            }
        }
    }
}

export_strategy!(SessionFlowReversal { exit_day: None, exit_submitted: false });
