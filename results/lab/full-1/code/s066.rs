use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, Timestamp,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const WINDOW_START: i64 = 9 * 60 + 30;
const SIGNAL_MINUTE: i64 = 9 * 60 + 45;
const EXIT_MINUTE: i64 = 16 * 60;
const WINDOW_LEN: usize = 15;
const WINDOW_MASK: u16 = (1_u16 << WINDOW_LEN) - 1;
const HISTORY_LEN: usize = 20;
const SIZING_MARGIN_PPM: i64 = 5_000;

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let adjusted_year = year - if month <= 2 { 1 } else { 0 };
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_piece = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_piece + 2) / 5 + 1;
    let month = month_piece + if month_piece < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

fn first_sunday(year: i64, month: i64) -> i64 {
    let first_day = days_from_civil(year, month, 1);
    let weekday_sunday_zero = (first_day + 4).rem_euclid(7);
    1 + (7 - weekday_sunday_zero) % 7
}

fn is_us_eastern_dst(utc_minute: i64) -> bool {
    let utc_day = utc_minute.div_euclid(1_440);
    let (year, _, _) = civil_from_days(utc_day);

    for candidate_year in [year - 1, year, year + 1] {
        let march_sunday = first_sunday(candidate_year, 3) + 7;
        let november_sunday = first_sunday(candidate_year, 11);
        let dst_start = days_from_civil(candidate_year, 3, march_sunday) * 1_440 + 7 * 60;
        let dst_end = days_from_civil(candidate_year, 11, november_sunday) * 1_440 + 6 * 60;
        if utc_minute >= dst_start && utc_minute < dst_end {
            return true;
        }
    }
    false
}

fn eastern_day_and_minute(timestamp: Timestamp) -> (i64, i64) {
    let minute_ns = TimeDelta::from_mins(1).as_nanos();
    let utc_minute = timestamp.as_nanos().div_euclid(minute_ns);
    let offset = if is_us_eastern_dst(utc_minute) { 4 * 60 } else { 5 * 60 };
    let local_minute = utc_minute + offset;
    (
        local_minute.div_euclid(1_440),
        local_minute.rem_euclid(1_440),
    )
}

fn is_weekday(day: i64) -> bool {
    // Unix epoch day zero was Thursday; Sunday is zero here.
    let weekday_sunday_zero = (day + 4).rem_euclid(7);
    weekday_sunday_zero != 0 && weekday_sunday_zero != 6
}

struct UsOpenFlowContinuation {
    current_day: Option<i64>,
    current_weekday: bool,
    window_mask: u16,
    window_volume: i128,
    first_open: Option<i64>,
    last_close: Option<i64>,
    history: VecDeque<i64>,
    entry_attempted: bool,
}

impl UsOpenFlowContinuation {
    fn new() -> Self {
        Self {
            current_day: None,
            current_weekday: false,
            window_mask: 0,
            window_volume: 0,
            first_open: None,
            last_close: None,
            history: VecDeque::with_capacity(HISTORY_LEN),
            entry_attempted: false,
        }
    }

    fn finalize_day(&mut self) {
        if self.current_weekday && self.window_mask == WINDOW_MASK {
            let volume = self.window_volume.clamp(0, i128::from(i64::MAX)) as i64;
            if self.history.len() == HISTORY_LEN {
                self.history.pop_front();
            }
            self.history.push_back(volume);
        }
    }

    fn reset_for_day(&mut self, day: i64) {
        self.current_day = Some(day);
        self.current_weekday = is_weekday(day);
        self.window_mask = 0;
        self.window_volume = 0;
        self.first_open = None;
        self.last_close = None;
        self.entry_attempted = false;
    }

    fn volume_meets_threshold(&self) -> bool {
        if self.history.len() < HISTORY_LEN {
            return false;
        }
        let mut sorted: Vec<i64> = self.history.iter().copied().collect();
        sorted.sort_unstable();
        let median_twice = i128::from(sorted[HISTORY_LEN / 2 - 1])
            + i128::from(sorted[HISTORY_LEN / 2]);
        self.window_volume * 8 >= median_twice * 5
    }
}

impl Strategy for UsOpenFlowContinuation {
    fn name(&self) -> String {
        "US Open Flow Continuation".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let (open_day, open_minute) = eastern_day_and_minute(bar.open_ts);
        if self.current_day != Some(open_day) {
            if self.current_day.is_some() {
                self.finalize_day();
            }
            self.reset_for_day(open_day);
        }

        if self.current_weekday && (WINDOW_START..WINDOW_START + WINDOW_LEN as i64).contains(&open_minute) {
            let slot = (open_minute - WINDOW_START) as u32;
            let bit = 1_u16 << slot;
            if self.window_mask & bit == 0 {
                self.window_mask |= bit;
                self.window_volume += i128::from(bar.volume.raw());
                if slot == 0 {
                    self.first_open = Some(bar.open.raw());
                }
                if slot == (WINDOW_LEN - 1) as u32 {
                    self.last_close = Some(bar.close.raw());
                }
            }
        }

        let (close_day, close_minute) = eastern_day_and_minute(bar.close_ts);
        if close_day != open_day {
            return;
        }

        if self.current_weekday
            && close_minute == SIGNAL_MINUTE
            && self.window_mask == WINDOW_MASK
            && !self.entry_attempted
        {
            let strong_return = match (self.first_open, self.last_close) {
                (Some(first), Some(last)) if first > 0 => {
                    i128::from(last - first) * 1_000 >= i128::from(first) * 2
                }
                _ => false,
            };

            if strong_return
                && self.volume_meets_threshold()
                && ctx.open_orders() == 0
                && ctx.position(INSTRUMENT).is_zero()
            {
                let qty = ctx.affordable_qty(
                    INSTRUMENT,
                    bar.close,
                    ctx.cash(),
                    SIZING_MARGIN_PPM,
                );
                if qty.is_positive() {
                    self.entry_attempted = true;
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                }
            }
        }

        if close_minute == EXIT_MINUTE && ctx.open_orders() == 0 {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
        }
    }
}

export_strategy!(UsOpenFlowContinuation::new());
