use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, Fill, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const HISTORY_WEEKS: usize = 20;
const SIZING_MARGIN_PPM: i64 = 5_000;
const WINDOW_START_MINUTE: i32 = 15 * 60 + 30;
const WINDOW_END_MINUTE: i32 = 15 * 60 + 59;

struct UsCloseFlowDrift {
    current_day: Option<i64>,
    window_start: Option<Price>,
    window_volume_raw: i128,
    window_count: usize,
    window_expected_minute: i32,
    window_valid: bool,
    prior_window_volumes: VecDeque<i128>,

    pending_entry_day: Option<i64>,
    pending_entry_ts: Option<i64>,
    entry_day: Option<i64>,
    exit_day: Option<i64>,
    last_exit_submit_day: Option<i64>,
    tracked_position_raw: i128,
    completed_round_trips: u64,
}

impl UsCloseFlowDrift {
    fn new() -> Self {
        Self {
            current_day: None,
            window_start: None,
            window_volume_raw: 0,
            window_count: 0,
            window_expected_minute: WINDOW_START_MINUTE,
            window_valid: false,
            prior_window_volumes: VecDeque::with_capacity(HISTORY_WEEKS),
            pending_entry_day: None,
            pending_entry_ts: None,
            entry_day: None,
            exit_day: None,
            last_exit_submit_day: None,
            tracked_position_raw: 0,
            completed_round_trips: 0,
        }
    }

    fn median_prior_volume(&self) -> Option<i128> {
        if self.prior_window_volumes.len() != HISTORY_WEEKS {
            return None;
        }
        let mut values: Vec<i128> = self.prior_window_volumes.iter().copied().collect();
        values.sort_unstable();
        Some((values[HISTORY_WEEKS / 2 - 1] + values[HISTORY_WEEKS / 2]) / 2)
    }

    fn record_window_volume(&mut self, volume_raw: i128) {
        self.prior_window_volumes.push_back(volume_raw);
        while self.prior_window_volumes.len() > HISTORY_WEEKS {
            self.prior_window_volumes.pop_front();
        }
    }

    fn reset_window(&mut self, day: i64) {
        self.current_day = Some(day);
        self.window_start = None;
        self.window_volume_raw = 0;
        self.window_count = 0;
        self.window_expected_minute = WINDOW_START_MINUTE;
        self.window_valid = false;
    }

    fn observe_window_bar(&mut self, minute: i32, bar: &Bar) -> bool {
        if minute == WINDOW_START_MINUTE {
            self.window_start = Some(bar.close);
            self.window_volume_raw = i128::from(bar.volume.raw());
            self.window_count = 1;
            self.window_expected_minute = WINDOW_START_MINUTE + 1;
            self.window_valid = true;
        } else if (WINDOW_START_MINUTE + 1..=WINDOW_END_MINUTE).contains(&minute)
            && self.window_start.is_some()
            && self.window_valid
        {
            if minute == self.window_expected_minute {
                self.window_volume_raw += i128::from(bar.volume.raw());
                self.window_count += 1;
                self.window_expected_minute += 1;
            } else {
                self.window_valid = false;
            }
        }

        minute == WINDOW_END_MINUTE
            && self.window_valid
            && self.window_count == 30
            && self.window_expected_minute == WINDOW_END_MINUTE + 1
    }

    fn weekday(day: i64) -> bool {
        let sunday_zero = (day + 4).rem_euclid(7);
        (1..=5).contains(&sunday_zero)
    }

    fn next_weekday(day: i64) -> i64 {
        let mut next = day + 1;
        while !Self::weekday(next) {
            next += 1;
        }
        next
    }
}

impl Strategy for UsCloseFlowDrift {
    fn name(&self) -> String {
        "U.S. Close Flow Drift".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let (day, minute) = new_york_day_minute(bar.close_ts.as_nanos());
        if self.current_day != Some(day) {
            self.reset_window(day);
        }

        // Market IOC orders should resolve on the next bar. Clear an unfilled
        // entry intent once that bar has passed so a later signal can be used.
        if self.pending_entry_day.is_some()
            && self.pending_entry_ts.is_some_and(|ts| bar.close_ts.as_nanos() > ts)
            && ctx.position(INSTRUMENT).is_zero()
            && ctx.open_orders() == 0
        {
            self.pending_entry_day = None;
            self.pending_entry_ts = None;
        }

        let complete_window = self.observe_window_bar(minute, bar);
        if !complete_window || !Self::weekday(day) {
            return;
        }

        let window_start = self.window_start.expect("complete window has a start");
        let window_volume_raw = self.window_volume_raw;
        let median_volume = self.median_prior_volume();
        let strong_return = i128::from(bar.close.raw()) * 1_000
            >= i128::from(window_start.raw()) * 1_002;
        let active_volume = median_volume
            .is_some_and(|median| window_volume_raw * 2 >= median * 3);

        // The exit order is sent after the 15:59 close, for the next bar's open.
        if self.exit_day.is_some_and(|exit_day| day >= exit_day)
            && ctx.position(INSTRUMENT).is_positive()
            && self.last_exit_submit_day != Some(day)
            && ctx.open_orders() == 0
        {
            let position = ctx.position(INSTRUMENT);
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            self.last_exit_submit_day = Some(day);
        } else if self.entry_day.is_none()
            && self.pending_entry_day.is_none()
            && ctx.position(INSTRUMENT).is_zero()
            && self.exit_day != Some(day)
            && strong_return
            && active_volume
            && window_volume_raw > 0
            && ctx.open_orders() == 0
        {
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
                self.pending_entry_day = Some(day);
                self.pending_entry_ts = Some(bar.close_ts.as_nanos());
            }
        }

        // Add today's completed window only after using the previous weekdays'
        // volumes to evaluate today's signal.
        self.record_window_volume(window_volume_raw);
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &Fill) {
        match fill.side {
            Side::Buy => {
                if self.tracked_position_raw == 0 {
                    let day = self
                        .pending_entry_day
                        .unwrap_or_else(|| new_york_day_minute(fill.ts.as_nanos()).0);
                    self.entry_day = Some(day);
                    self.exit_day = Some(Self::next_weekday(day));
                    self.last_exit_submit_day = None;
                }
                self.tracked_position_raw += i128::from(fill.qty.raw());
                self.pending_entry_day = None;
                self.pending_entry_ts = None;
            }
            Side::Sell => {
                self.tracked_position_raw -= i128::from(fill.qty.raw());
                if self.tracked_position_raw <= 0 && self.entry_day.is_some() {
                    self.completed_round_trips += 1;
                    self.tracked_position_raw = 0;
                    self.entry_day = None;
                    self.exit_day = None;
                    self.last_exit_submit_day = None;
                }
            }
        }
    }

    fn on_finish(&mut self, ctx: &mut Ctx) {
        if self.completed_round_trips >= 30 {
            ctx.log(&format!(
                "U.S. Close Flow Drift completed {} round trips; minimum trade-count threshold met.",
                self.completed_round_trips
            ));
        } else {
            ctx.log(&format!(
                "U.S. Close Flow Drift completed {} round trips; fewer than 30, so results are not informative.",
                self.completed_round_trips
            ));
        }
    }
}

fn new_york_day_minute(timestamp_ns: i64) -> (i64, i32) {
    let day_ns = i128::from(TimeDelta::from_days(1).as_nanos());
    let hour_ns = i128::from(TimeDelta::from_hours(1).as_nanos());
    let minute_ns = i128::from(TimeDelta::from_mins(1).as_nanos());
    let utc_ns = i128::from(timestamp_ns);
    let utc_day = (utc_ns.div_euclid(day_ns)) as i64;

    let (year, _, _) = civil_from_days(utc_day);
    let march_second_sunday = nth_sunday_of_month(year, 3, 2);
    let november_first_sunday = nth_sunday_of_month(year, 11, 1);
    let dst_start = i128::from(days_from_civil(year, 3, march_second_sunday)) * day_ns
        + 7 * hour_ns;
    let dst_end = i128::from(days_from_civil(year, 11, november_first_sunday)) * day_ns
        + 6 * hour_ns;
    let offset_hours = if utc_ns >= dst_start && utc_ns < dst_end {
        4
    } else {
        5
    };

    let local_ns = utc_ns - i128::from(offset_hours) * hour_ns;
    let local_day = local_ns.div_euclid(day_ns) as i64;
    let minute = (local_ns.rem_euclid(day_ns) / minute_ns) as i32;
    (local_day, minute)
}

fn nth_sunday_of_month(year: i64, month: i64, occurrence: i64) -> i64 {
    let first_day = days_from_civil(year, month, 1);
    let weekday_sunday_zero = (first_day + 4).rem_euclid(7);
    let first_sunday = 1 + (7 - weekday_sunday_zero).rem_euclid(7);
    first_sunday + 7 * (occurrence - 1)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let adjusted_year = year - if month <= 2 { 1 } else { 0 };
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 {
        z / 146_097
    } else {
        (z - 146_096) / 146_097
    };
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524
        - day_of_era / 146_096)
        / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_part = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = month_part + if month_part < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

export_strategy!(UsCloseFlowDrift::new());
