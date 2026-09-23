use tape_guest::{Bar, Ctx, Fill, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

struct MonthEndWinnerRebalance {
    started: bool,
    current_month: Option<(i64, i64)>,
    month_open: Option<Price>,
    sold_this_month: bool,
}

impl MonthEndWinnerRebalance {
    fn utc_date(ts: i64) -> (i64, i64, i64) {
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let days = ts.div_euclid(day_ns);
        Self::civil_from_days(days)
    }

    // Gregorian civil date conversion, with days counted from 1970-01-01.
    fn civil_from_days(days: i64) -> (i64, i64, i64) {
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
        let mut year = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = mp + if mp < 10 { 3 } else { -9 };
        year += if month <= 2 { 1 } else { 0 };
        (year, month, day)
    }

    fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
        let y = if month <= 2 { year - 1 } else { year };
        let era = if y >= 0 { y } else { y - 399 }.div_euclid(400);
        let yoe = y - era * 400;
        let adjusted_month = month + if month > 2 { -3 } else { 9 };
        let doy = (153 * adjusted_month + 2) / 5 + day - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    fn days_in_month(year: i64, month: i64) -> i64 {
        match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 => {
                if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
                    29
                } else {
                    28
                }
            }
            _ => 30,
        }
    }

    fn is_new_york_1559(ts: i64, year: i64, month: i64, day: i64) -> bool {
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let time_of_day_ns = ts.rem_euclid(day_ns);
        let weekday = |absolute_day: i64| (absolute_day + 4).rem_euclid(7); // Sunday = 3

        // Current U.S. daylight-saving rule: second Sunday in March to first Sunday in November.
        let march_first_weekday = weekday(Self::days_from_civil(year, 3, 1));
        let second_sunday_march = 1 + (3 - march_first_weekday).rem_euclid(7) + 7;
        let november_first_weekday = weekday(Self::days_from_civil(year, 11, 1));
        let first_sunday_november = 1 + (3 - november_first_weekday).rem_euclid(7);

        let daylight = if month < 3 || month > 11 {
            false
        } else if (4..=10).contains(&month) {
            true
        } else if month == 3 {
            day > second_sunday_march
                || (day == second_sunday_march
                    && time_of_day_ns >= TimeDelta::from_hours(7).as_nanos())
        } else {
            day < first_sunday_november
                || (day == first_sunday_november
                    && time_of_day_ns < TimeDelta::from_hours(6).as_nanos())
        };

        let utc_minute = time_of_day_ns / minute_ns;
        let offset_minutes = if daylight { 4 * 60 } else { 5 * 60 };
        (utc_minute - offset_minutes).rem_euclid(1_440) == 15 * 60 + 59
    }
}

impl Strategy for MonthEndWinnerRebalance {
    fn name(&self) -> String {
        "Month-End Winner Rebalance".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let (year, month, day) = Self::utc_date(bar.close_ts.as_nanos());
        if self.current_month != Some((year, month)) {
            self.current_month = Some((year, month));
            self.month_open = Some(bar.open);
            self.sold_this_month = false;
        }

        if !self.started {
            self.started = true;
            if ctx.open_orders() == 0 {
                let qty = ctx.affordable_qty(
                    INSTRUMENT,
                    bar.open,
                    ctx.cash(),
                    SIZING_MARGIN_PPM,
                );
                if qty.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                    return;
                }
            }
        }

        let is_month_end = day == Self::days_in_month(year, month);
        if is_month_end
            && Self::is_new_york_1559(bar.close_ts.as_nanos(), year, month, day)
            && self.month_open.is_some_and(|open| bar.close > open)
            && ctx.position(INSTRUMENT).is_positive()
            && ctx.open_orders() == 0
        {
            let position = ctx.position(INSTRUMENT);
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            return;
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let utc_minute = bar.close_ts.as_nanos().rem_euclid(day_ns) / minute_ns;
        if is_month_end
            && utc_minute == 1_439
            && self.sold_this_month
            && ctx.open_orders() == 0
        {
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
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Sell {
            self.sold_this_month = true;
        }
    }
}

export_strategy!(MonthEndWinnerRebalance {
    started: false,
    current_month: None,
    month_open: None,
    sold_this_month: false,
});
