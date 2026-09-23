use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, Timestamp, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const WINDOW_START: u16 = 570; // 09:30
const WINDOW_END: u16 = 584; // 09:44
const EXIT_SIGNAL_MINUTE: u16 = 824; // 13:44, submit for the 13:45 open
const HISTORY_LEN: usize = 20;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct Window {
    valid: bool,
    next_minute: u16,
    count: u8,
    volume_raw: i128,
    start_open: Price,
    end_close: Price,
}

struct UsOpenFlowContinuation {
    history: VecDeque<i128>,
    current_day: Option<i64>,
    window: Option<Window>,
}

impl UsOpenFlowContinuation {
    fn is_weekday(day_of_week: i64) -> bool {
        // Unix-day convention from the SDK: 0 = Thursday.
        matches!(day_of_week, 4 | 5 | 6 | 0 | 1)
    }

    fn remember_volume(&mut self, volume_raw: i128) {
        self.history.push_back(volume_raw);
        if self.history.len() > HISTORY_LEN {
            self.history.pop_front();
        }
    }

    fn volume_threshold_met(&self, current_volume: i128) -> bool {
        if self.history.len() != HISTORY_LEN {
            return false;
        }
        let mut values: Vec<i128> = self.history.iter().copied().collect();
        values.sort_unstable();
        // For 20 observations the median is the average of the middle two.
        // Compare without rounding: current >= 1.25 * median.
        current_volume * 8 >= (values[9] + values[10]) * 5
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

        let (local_day, minute, day_of_week) = new_york_clock(bar.open_ts);
        if self.current_day != Some(local_day) {
            self.current_day = Some(local_day);
            self.window = None;
        }

        if !Self::is_weekday(day_of_week) {
            return;
        }

        if minute == WINDOW_START {
            self.window = Some(Window {
                valid: true,
                next_minute: WINDOW_START + 1,
                count: 1,
                volume_raw: i128::from(bar.volume.raw()),
                start_open: bar.open,
                end_close: bar.close,
            });
        } else if (WINDOW_START + 1..=WINDOW_END).contains(&minute) {
            if let Some(window) = self.window.as_mut() {
                if window.valid && minute == window.next_minute {
                    window.next_minute += 1;
                    window.count += 1;
                    window.volume_raw += i128::from(bar.volume.raw());
                    window.end_close = bar.close;
                } else {
                    window.valid = false;
                }
            }
        }

        // The 09:44 bar has now closed. Submit an entry for the next bar's open.
        if minute == WINDOW_END {
            let completed = self.window.as_ref().and_then(|window| {
                (window.valid && window.count == 15).then_some((
                    window.volume_raw,
                    window.start_open,
                    window.end_close,
                ))
            });

            if let Some((volume, start_open, end_close)) = completed {
                let enough_history = self.history.len() == HISTORY_LEN;
                let volume_ok = self.volume_threshold_met(volume);
                let return_ok = start_open.raw() > 0
                    && i128::from(end_close.raw()) * 1_000
                        >= i128::from(start_open.raw()) * 1_002;

                if enough_history
                    && volume_ok
                    && return_ok
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
                        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                    }
                }

                // Only complete weekday windows are observations. Evaluate the
                // signal first so today's volume is never included in its baseline.
                self.remember_volume(volume);
            }
        }

        // The 13:44 bar has closed, so a market order is submitted for 13:45.
        if minute == EXIT_SIGNAL_MINUTE
            && ctx.open_orders() == 0
        {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
        }
    }
}

fn new_york_clock(timestamp: Timestamp) -> (i64, u16, i64) {
    let minute_ns = TimeDelta::from_mins(1).as_nanos();
    let utc_minute = timestamp.as_nanos().div_euclid(minute_ns);
    let utc_day = utc_minute.div_euclid(1_440);
    let (year, _, _) = civil_from_days(utc_day);

    // New York switches at 07:00 UTC on the second Sunday in March and
    // 06:00 UTC on the first Sunday in November.
    let march_first = days_from_civil(year, 3, 1);
    let march_first_weekday = (march_first + 4).rem_euclid(7); // Sunday = 0
    let second_sunday_march = 1 + (7 - march_first_weekday).rem_euclid(7) + 7;
    let dst_start = days_from_civil(year, 3, second_sunday_march) * 1_440 + 7 * 60;

    let november_first = days_from_civil(year, 11, 1);
    let november_first_weekday = (november_first + 4).rem_euclid(7);
    let first_sunday_november = 1 + (7 - november_first_weekday).rem_euclid(7);
    let dst_end = days_from_civil(year, 11, first_sunday_november) * 1_440 + 6 * 60;

    let offset_minutes = if utc_minute >= dst_start && utc_minute < dst_end {
        -4 * 60
    } else {
        -5 * 60
    };
    let local_minute = utc_minute + offset_minutes;
    let local_day = local_minute.div_euclid(1_440);
    let minute_of_day = local_minute.rem_euclid(1_440) as u16;
    let day_of_week = (local_day + 4).rem_euclid(7);
    (local_day, minute_of_day, day_of_week)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let adjusted_year = year - i64::from(month <= 2);
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
    (year, month, day)
}

export_strategy!(UsOpenFlowContinuation {
    history: VecDeque::with_capacity(HISTORY_LEN),
    current_day: None,
    window: None,
});
