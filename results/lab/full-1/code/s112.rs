use tape_guest::{
    Bar, Ctx, Fill, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, Timestamp,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

struct MonthEndRebalanceSnapback {
    entry_ts_ns: Option<i64>,
}

impl Strategy for MonthEndRebalanceSnapback {
    fn name(&self) -> String {
        "month-end-rebalance-snapback".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_zero() {
            self.entry_ts_ns = None;
        }

        // An order submitted after this bar closes can fill at the following bar's open.
        if position.is_positive() {
            let Some(entry_ts_ns) = self.entry_ts_ns else {
                return;
            };
            let six_hours_ns = TimeDelta::from_hours(6).as_nanos();
            if bar.close_ts.as_nanos() >= entry_ts_ns.saturating_add(six_hours_ns)
                && ctx.open_orders() == 0
            {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
            return;
        }

        // Do not enter when already holding any position, or while an order is outstanding.
        if !position.is_zero() || ctx.open_orders() != 0 {
            return;
        }

        let Some(local_day) = new_york_day_at_1600(bar.close_ts) else {
            return;
        };
        if !is_one_of_last_two_exchange_sessions(local_day) {
            return;
        }

        // Reserve for the specified 0.02% slippage when sizing. The execution engine
        // remains responsible for applying actual fees, slippage, delay, and volume caps.
        let estimated_buy_price = bar.close.mul_ratio(10_002, 10_000);
        let qty = ctx.affordable_qty(
            INSTRUMENT,
            estimated_buy_price,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        if qty.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &Fill) {
        if fill.order_id == fill.order_id && fill.qty.is_positive() {
            match fill.side {
                Side::Buy => {
                    if self.entry_ts_ns.is_none() {
                        self.entry_ts_ns = Some(fill.ts.as_nanos());
                    }
                }
                Side::Sell => {
                    if ctx.position(INSTRUMENT).is_zero() {
                        self.entry_ts_ns = None;
                    }
                }
            }
        }
    }
}

// Returns the local calendar day only when the timestamp is exactly 16:00
// America/New_York time.
fn new_york_day_at_1600(ts: Timestamp) -> Option<i64> {
    let utc_ns = ts.as_nanos();
    let utc_day_ns = TimeDelta::from_days(1).as_nanos();
    let minute_ns = TimeDelta::from_mins(1).as_nanos();
    let utc_day = utc_ns.div_euclid(utc_day_ns);
    let (year, _, _) = civil_from_days(utc_day);
    let offset_minutes = if is_new_york_dst(utc_ns, year, utc_day_ns) {
        4 * 60
    } else {
        5 * 60
    };
    let local_ns = utc_ns.saturating_add(i64::from(offset_minutes) * minute_ns);
    if local_ns.rem_euclid(minute_ns) != 0 {
        return None;
    }
    let local_minute = local_ns.rem_euclid(utc_day_ns) / minute_ns;
    (local_minute == 16 * 60).then_some(local_ns.div_euclid(utc_day_ns))
}

fn is_new_york_dst(utc_ns: i64, year: i64, day_ns: i64) -> bool {
    let (start_month, start_day, end_month, end_day) = if year >= 2007 {
        (
            3,
            nth_weekday_of_month(year, 3, 0, 2),
            11,
            nth_weekday_of_month(year, 11, 0, 1),
        )
    } else if year >= 1987 {
        (
            4,
            nth_weekday_of_month(year, 4, 0, 1),
            10,
            last_weekday_of_month(year, 10, 0),
        )
    } else if year >= 1967 {
        let start = if year == 1974 {
            days_from_civil(year, 1, 6)
        } else if year == 1975 {
            days_from_civil(year, 2, 23)
        } else {
            last_weekday_of_month(year, 4, 0)
        };
        let (start_year, start_month, start_day) = civil_from_days(start);
        let _ = start_year;
        (
            start_month,
            start_day,
            10,
            last_weekday_of_month(year, 10, 0),
        )
    } else {
        return false;
    };

    let start_day = days_from_civil(year, start_month, start_day);
    let end_day = days_from_civil(year, end_month, end_day);
    // U.S. transitions occur at 02:00 local standard time in spring (07:00 UTC)
    // and 02:00 local daylight time in autumn (06:00 UTC).
    let hour_ns = TimeDelta::from_hours(1).as_nanos();
    let start_utc = start_day * day_ns + 7 * hour_ns;
    let end_utc = end_day * day_ns + 6 * hour_ns;
    utc_ns >= start_utc && utc_ns < end_utc
}

fn is_one_of_last_two_exchange_sessions(day: i64) -> bool {
    let (year, month, day_of_month) = civil_from_days(day);
    if !is_exchange_session(day) {
        return false;
    }
    let month_length = days_in_month(year, month);
    let mut sessions_remaining = 0;
    for date in day_of_month..=month_length {
        if is_exchange_session(days_from_civil(year, month, date)) {
            sessions_remaining += 1;
        }
    }
    sessions_remaining <= 2
}

fn is_exchange_session(day: i64) -> bool {
    let weekday = weekday_sunday_zero(day);
    weekday != 0 && weekday != 6 && !is_nyse_holiday(day)
}

fn is_nyse_holiday(day: i64) -> bool {
    let (year, month, date) = civil_from_days(day);
    let weekday = weekday_sunday_zero(day);

    // New Year's Day; NYSE does not generally observe a Saturday New Year's Day
    // on the preceding Friday.
    if (month == 1 && date == 1)
        || (month == 1 && date == 2 && weekday == 1 && weekday_sunday_zero(day - 1) == 0)
    {
        return true;
    }

    // Martin Luther King Jr. Day and Washington's Birthday.
    if (month == 1 && day == nth_weekday_of_month(year, 1, 1, 3))
        || (month == 2 && day == nth_weekday_of_month(year, 2, 1, 3))
    {
        return true;
    }

    // Good Friday (two days before Gregorian Easter Sunday).
    if day == easter_sunday(year) - 2 {
        return true;
    }

    // Memorial Day, Independence Day and its weekday observance.
    if (month == 5 && day == last_weekday_of_month(year, 5, 1))
        || (month == 7 && date == 4)
        || (month == 7 && date == 3 && weekday == 5 && weekday_sunday_zero(day + 1) == 6)
        || (month == 7 && date == 5 && weekday == 1 && weekday_sunday_zero(day - 1) == 0)
    {
        return true;
    }

    // Juneteenth became an NYSE holiday in 2022.
    if year >= 2022
        && ((month == 6 && date == 19)
            || (month == 6 && date == 18 && weekday == 5 && weekday_sunday_zero(day + 1) == 6)
            || (month == 6 && date == 20 && weekday == 1 && weekday_sunday_zero(day - 1) == 0))
    {
        return true;
    }

    // Labor Day and Thanksgiving.
    if (month == 9 && day == nth_weekday_of_month(year, 9, 1, 1))
        || (month == 11 && day == nth_weekday_of_month(year, 11, 4, 4))
    {
        return true;
    }

    // Christmas and its weekday observance.
    (month == 12 && date == 25)
        || (month == 12 && date == 24 && weekday == 5 && weekday_sunday_zero(day + 1) == 6)
        || (month == 12 && date == 26 && weekday == 1 && weekday_sunday_zero(day - 1) == 0)
}

fn nth_weekday_of_month(year: i64, month: i64, weekday: i64, nth: i64) -> i64 {
    let first = days_from_civil(year, month, 1);
    first + (weekday - weekday_sunday_zero(first)).rem_euclid(7) + 7 * (nth - 1)
}

fn last_weekday_of_month(year: i64, month: i64, weekday: i64) -> i64 {
    let last = days_from_civil(year, month, days_in_month(year, month));
    last - (weekday_sunday_zero(last) - weekday).rem_euclid(7)
}

fn weekday_sunday_zero(day: i64) -> i64 {
    (day + 4).rem_euclid(7)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if is_leap_year(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn is_leap_year(year: i64) -> bool {
    year.rem_euclid(4) == 0 && (year.rem_euclid(100) != 0 || year.rem_euclid(400) == 0)
}

fn easter_sunday(year: i64) -> i64 {
    let a = year.rem_euclid(19);
    let b = year.div_euclid(100);
    let c = year.rem_euclid(100);
    let d = b / 4;
    let e = b.rem_euclid(4);
    let f = (b + 8) / 25;
    let g = (b - f + 1) / 3;
    let h = (19 * a + b - d - g + 15).rem_euclid(30);
    let i = c / 4;
    let k = c.rem_euclid(4);
    let l = (32 + 2 * e + 2 * i - h - k).rem_euclid(7);
    let m = (a + 11 * h + 22 * l) / 451;
    let month = (h + l - 7 * m + 114) / 31;
    let date = (h + l - 7 * m + 114).rem_euclid(31) + 1;
    days_from_civil(year, month, date)
}

fn days_from_civil(year: i64, month: i64, date: i64) -> i64 {
    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + date - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted_days = days + 719_468;
    let era = shifted_days.div_euclid(146_097);
    let day_of_era = shifted_days - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524
        - day_of_era / 146_096)
        / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_part = (5 * day_of_year + 2) / 153;
    let date = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = month_part + if month_part < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, date)
}

export_strategy!(MonthEndRebalanceSnapback { entry_ts_ns: None });
