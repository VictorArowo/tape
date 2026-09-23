use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const HISTORY_DAYS: usize = 20;
const MARGIN_PPM: i64 = 5_000;

fn days_from_civil(mut year: i64, month: i64, day: i64) -> i64 {
    year -= i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 { shifted } else { shifted - 146_096 } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_part = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = month_part + if month_part < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

// US daylight-saving transitions since 2007: second Sunday in March at 07:00 UTC
// through first Sunday in November at 06:00 UTC.
fn is_new_york_dst(utc_minute: i64) -> bool {
    let utc_day = utc_minute.div_euclid(1_440);
    let (year, _, _) = civil_from_days(utc_day);
    let march_first = days_from_civil(year, 3, 1);
    let november_first = days_from_civil(year, 11, 1);
    let march_sunday = 1 + (3 - march_first.rem_euclid(7)).rem_euclid(7) + 7;
    let november_sunday = 1 + (3 - november_first.rem_euclid(7)).rem_euclid(7);
    let start = days_from_civil(year, 3, march_sunday) * 1_440 + 7 * 60;
    let end = days_from_civil(year, 11, november_sunday) * 1_440 + 6 * 60;
    utc_minute >= start && utc_minute < end
}

fn new_york_day_and_minute(ts: tape_guest::Timestamp) -> (i64, i64) {
    let minute_ns = TimeDelta::from_mins(1).as_nanos();
    let utc_minute = ts.as_nanos().div_euclid(minute_ns);
    let offset = if is_new_york_dst(utc_minute) { 4 * 60 } else { 5 * 60 };
    let local_minute = utc_minute - offset;
    (local_minute.div_euclid(1_440), local_minute.rem_euclid(1_440))
}

fn is_weekday(day: i64) -> bool {
    let weekday = day.rem_euclid(7); // 0 = Thursday
    weekday != 2 && weekday != 3
}

struct OpenFlowCarry {
    history: VecDeque<i128>,
    current_day: Option<i64>,
    session_open: Option<tape_guest::Price>,
    session_volume: i128,
    session_count: usize,
    expected_minute: i64,
    session_valid: bool,
    pending_entry_day: Option<i64>,
    attempted_day: Option<i64>,
    exit_submitted_day: Option<i64>,
}

impl OpenFlowCarry {
    fn new() -> Self {
        Self {
            history: VecDeque::with_capacity(HISTORY_DAYS),
            current_day: None,
            session_open: None,
            session_volume: 0,
            session_count: 0,
            expected_minute: 570,
            session_valid: false,
            pending_entry_day: None,
            attempted_day: None,
            exit_submitted_day: None,
        }
    }

    fn start_day(&mut self, day: i64) {
        self.current_day = Some(day);
        self.session_open = None;
        self.session_volume = 0;
        self.session_count = 0;
        self.expected_minute = 570;
        self.session_valid = false;
        if self.pending_entry_day.is_some_and(|pending| pending != day) {
            self.pending_entry_day = None;
        }
    }

    fn median_threshold_met(&self, volume: i128) -> bool {
        if self.history.len() != HISTORY_DAYS {
            return false;
        }
        let mut sorted: Vec<i128> = self.history.iter().copied().collect();
        sorted.sort_unstable();
        // For an even-sized sample, median is the mean of the two middle values.
        // Compare volume >= 1.5 * median without rounding that mean.
        volume * 4 >= 3 * (sorted[HISTORY_DAYS / 2 - 1] + sorted[HISTORY_DAYS / 2])
    }

    fn finish_open_interval(&mut self, day: i64, close: tape_guest::Price) {
        if !self.session_valid || self.session_count != 30 {
            return;
        }
        let Some(open) = self.session_open else {
            return;
        };
        let volume = self.session_volume;
        let return_is_large_enough = open.is_positive()
            && i128::from(close.raw()) * 10_000 >= i128::from(open.raw()) * 10_015;

        // Evaluate against only the preceding completed weekday intervals.
        if return_is_large_enough
            && self.median_threshold_met(volume)
            && self.attempted_day != Some(day)
        {
            self.attempted_day = Some(day);
            self.pending_entry_day = Some(day);
        }

        self.history.push_back(volume);
        if self.history.len() > HISTORY_DAYS {
            self.history.pop_front();
        }
    }

    fn try_entry(&mut self, ctx: &mut Ctx, day: i64, bar: &Bar) {
        if self.pending_entry_day != Some(day) {
            return;
        }
        // The 10:00 bar's volume is only known after it closes. Use it to enforce
        // the participation cap without looking ahead, then submit for the next open.
        self.pending_entry_day = None;
        if ctx.open_orders() != 0 || !ctx.position(INSTRUMENT).is_zero() || !bar.open.is_positive() {
            return;
        }

        let budget = ctx.cash().mul_ratio(95, 100);
        let affordable = ctx.affordable_qty(INSTRUMENT, bar.open, budget, MARGIN_PPM);
        let volume_cap = Qty::from_raw(bar.volume.raw().max(0) / 10);
        let qty = Qty::from_raw(affordable.raw().min(volume_cap.raw()));
        if qty.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }
}

impl Strategy for OpenFlowCarry {
    fn name(&self) -> String {
        "U.S. Open Flow Carry".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        // Use the bar's opening timestamp to identify its one-minute interval.
        let (day, minute) = new_york_day_and_minute(bar.open_ts);
        if self.current_day != Some(day) {
            self.start_day(day);
        }

        if is_weekday(day) {
            if minute == 570 {
                self.session_open = Some(bar.open);
                self.session_volume = i128::from(bar.volume.raw().max(0));
                self.session_count = 1;
                self.expected_minute = 571;
                self.session_valid = true;
            } else if (571..=599).contains(&minute) && self.session_open.is_some() {
                if self.session_valid && minute == self.expected_minute {
                    self.session_volume += i128::from(bar.volume.raw().max(0));
                    self.session_count += 1;
                    self.expected_minute = minute + 1;
                } else {
                    self.session_valid = false;
                }
            }

            if minute == 599 {
                self.finish_open_interval(day, bar.close);
            }
        }

        if minute == 600 {
            self.try_entry(ctx, day, bar);
        } else if minute > 600 && self.pending_entry_day == Some(day) {
            // Do not enter late if the 10:00 bar was absent.
            self.pending_entry_day = None;
        }

        // Submitted after the 15:59 interval closes; the market order is intended
        // to execute at the following (16:00) bar open.
        if minute == 959
            && is_weekday(day)
            && self.exit_submitted_day != Some(day)
            && ctx.open_orders() == 0
        {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                self.exit_submitted_day = Some(day);
            }
        }
    }
}

export_strategy!(OpenFlowCarry::new());
