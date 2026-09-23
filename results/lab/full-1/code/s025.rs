use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy)]
struct CivilDate {
    year: i64,
    month: i64,
    day: i64,
}

fn civil_date_from_days(days_since_epoch: i64) -> CivilDate {
    // Convert Unix-epoch days to a proleptic Gregorian calendar date.
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524
        - day_of_era / 146_096)
        / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era
        - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_part = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = month_part + if month_part < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);

    CivilDate { year, month, day }
}

fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 => if is_leap_year(year) { 29 } else { 28 },
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

// Timestamp weekday numbering is 0 = Thursday, so weekdays are 0, 1, 4, 5, 6.
fn is_monday_to_friday(days_since_epoch: i64) -> bool {
    matches!(days_since_epoch.rem_euclid(7), 0 | 1 | 4 | 5 | 6)
}

fn is_last_weekday_of_month(days_since_epoch: i64, date: CivilDate) -> bool {
    if !is_monday_to_friday(days_since_epoch) {
        return false;
    }

    let last_day = days_in_month(date.year, date.month);
    for offset in 1..=(last_day - date.day) {
        if is_monday_to_friday(days_since_epoch + offset) {
            return false;
        }
    }
    true
}

struct MonthTurnRebalancingWindow {
    exit_day: Option<i64>,
}

impl Strategy for MonthTurnRebalancingWindow {
    fn name(&self) -> String {
        "month-turn-rebalancing-window".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let timestamp_ns = bar.close_ts.as_nanos();
        let nanos_per_day = TimeDelta::from_days(1).as_nanos();
        let nanos_per_minute = TimeDelta::from_mins(1).as_nanos();
        let day = timestamp_ns.div_euclid(nanos_per_day);
        let minute_of_day = timestamp_ns
            .div_euclid(nanos_per_minute)
            .rem_euclid(1_440);
        let position = ctx.position(INSTRUMENT);

        if position.is_positive() {
            if let Some(exit_day) = self.exit_day {
                if day >= exit_day && ctx.open_orders() == 0 {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                }
            }
            return;
        }

        if self.exit_day.is_some_and(|exit_day| day >= exit_day) && ctx.open_orders() == 0 {
            self.exit_day = None;
        }

        if minute_of_day != 0 || ctx.open_orders() != 0 {
            return;
        }

        let date = civil_date_from_days(day);
        if !is_last_weekday_of_month(day, date) {
            return;
        }

        let qty = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        if qty.is_positive() {
            let next_month_second_day = day + (days_in_month(date.year, date.month) - date.day) + 2;
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            self.exit_day = Some(next_month_second_day);
        }
    }
}

export_strategy!(MonthTurnRebalancingWindow { exit_day: None });
