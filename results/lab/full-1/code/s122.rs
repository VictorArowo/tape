use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, Timestamp, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const HISTORY_DAYS: usize = 20;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct UsOpenFlow {
    current_day: Option<i64>,
    prior_volumes: VecDeque<i128>,
    opening_day: Option<i64>,
    opening_count: usize,
    opening_start_raw: i64,
    opening_last_close_raw: i64,
    opening_volume: i128,
    opening_valid: bool,
    opening_processed: bool,
    entry_active: bool,
    exit_active: bool,
}

impl UsOpenFlow {
    fn reset_opening_window(&mut self, day: i64, bar: &Bar) {
        self.opening_day = Some(day);
        self.opening_count = 1;
        self.opening_start_raw = bar.open.raw();
        self.opening_last_close_raw = bar.close.raw();
        self.opening_volume = i128::from(bar.volume.raw());
        self.opening_valid = true;
        self.opening_processed = false;
    }

    fn record_opening_window(&mut self, volume: i128) {
        self.prior_volumes.push_back(volume);
        if self.prior_volumes.len() > HISTORY_DAYS {
            self.prior_volumes.pop_front();
        }
    }

    fn volume_is_unusually_high(&self, volume: i128) -> bool {
        if self.prior_volumes.len() != HISTORY_DAYS {
            return false;
        }
        let mut sorted: Vec<i128> = self.prior_volumes.iter().copied().collect();
        sorted.sort_unstable();
        let middle_sum = sorted[HISTORY_DAYS / 2 - 1] + sorted[HISTORY_DAYS / 2];
        // For an even-sized sample, the median is the average of its two middle values.
        // Compare exactly against 1.25 times that median.
        volume * 8 >= middle_sum * 5
    }

    fn return_meets_threshold(&self) -> bool {
        self.opening_start_raw > 0
            && i128::from(self.opening_last_close_raw) * 10_000
                >= i128::from(self.opening_start_raw) * 10_035
    }
}

impl Strategy for UsOpenFlow {
    fn name(&self) -> String {
        "U.S. Open Flow Continuation".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        // Use the bar's open timestamp so the 09:59 bar is acted on after it
        // closes, placing an order for the following (normally 10:00) open.
        let (day, minute, weekday) = new_york_time(bar.open_ts);
        if self.current_day != Some(day) {
            self.current_day = Some(day);
            self.entry_active = false;
            self.opening_day = None;
            self.opening_count = 0;
            self.opening_valid = false;
            self.opening_processed = false;
        }

        if weekday {
            if minute == 9 * 60 + 30 {
                self.reset_opening_window(day, bar);
            } else if (9 * 60 + 31..=9 * 60 + 59).contains(&minute)
                && self.opening_day == Some(day)
            {
                let expected_minute = 9 * 60 + 30 + self.opening_count as i64;
                if self.opening_valid && minute == expected_minute {
                    self.opening_count += 1;
                    self.opening_last_close_raw = bar.close.raw();
                    self.opening_volume += i128::from(bar.volume.raw());
                } else {
                    self.opening_valid = false;
                }
            }

            if minute == 9 * 60 + 59
                && self.opening_day == Some(day)
                && !self.opening_processed
            {
                self.opening_processed = true;
                if self.opening_valid && self.opening_count == 30 {
                    let volume = self.opening_volume;
                    let signal = self.return_meets_threshold()
                        && self.volume_is_unusually_high(volume);
                    if signal
                        && ctx.position(INSTRUMENT).is_zero()
                        && !self.exit_active
                    {
                        self.entry_active = true;
                    }
                    // Evaluate the signal against prior sessions, then include
                    // today's completed window in the history for tomorrow.
                    self.record_opening_window(volume);
                }
            }

            // The 15:59 bar has just closed; a market order submitted now is
            // intended for the 16:00 open. Keep retrying any residual position.
            if minute == 15 * 60 + 59 {
                self.entry_active = false;
                self.exit_active = true;
            }
        }

        if ctx.open_orders() != 0 {
            return;
        }

        if self.exit_active {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            } else if position.is_zero() {
                self.exit_active = false;
            }
            return;
        }

        if self.entry_active {
            let price = bar.close;
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                price,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        }
    }
}

fn new_york_time(timestamp: Timestamp) -> (i64, i64, bool) {
    let nanos_per_second = TimeDelta::from_mins(1).as_nanos() / 60;
    let utc_seconds = timestamp.as_nanos().div_euclid(nanos_per_second);
    let utc_day = utc_seconds.div_euclid(86_400);
    let year = civil_year_from_days(utc_day);

    // U.S. daylight-saving time starts at 02:00 EST (07:00 UTC) on the
    // second Sunday in March and ends at 02:00 EDT (06:00 UTC) on the
    // first Sunday in November.
    let march_first = days_from_civil(year, 3, 1);
    let march_first_weekday = march_first.rem_euclid(7); // 0 = Thursday
    let march_first_sunday = (3 - march_first_weekday).rem_euclid(7);
    let dst_start_day = march_first + march_first_sunday + 7;
    let dst_start = dst_start_day * 86_400 + 7 * 3_600;

    let november_first = days_from_civil(year, 11, 1);
    let november_first_weekday = november_first.rem_euclid(7);
    let november_first_sunday = (3 - november_first_weekday).rem_euclid(7);
    let dst_end_day = november_first + november_first_sunday;
    let dst_end = dst_end_day * 86_400 + 6 * 3_600;

    let is_dst = utc_seconds >= dst_start && utc_seconds < dst_end;
    let offset_seconds = if is_dst { 4 * 3_600 } else { 5 * 3_600 };
    let local_seconds = utc_seconds - offset_seconds;
    let local_day = local_seconds.div_euclid(86_400);
    let minute_of_day = local_seconds.rem_euclid(86_400) / 60;
    let weekday_index = local_day.rem_euclid(7); // 0 = Thursday
    let weekday = weekday_index != 2 && weekday_index != 3;

    (local_day, minute_of_day, weekday)
}

fn civil_year_from_days(days: i64) -> i64 {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524
        - day_of_era / 146_096)
        / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era
        - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    if month <= 2 {
        year += 1;
    }
    year
}

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
    era * 146_097 + day_of_era - 719_468
}

export_strategy!(UsOpenFlow {
    current_day: None,
    prior_volumes: VecDeque::with_capacity(HISTORY_DAYS),
    opening_day: None,
    opening_count: 0,
    opening_start_raw: 0,
    opening_last_close_raw: 0,
    opening_volume: 0,
    opening_valid: false,
    opening_processed: false,
    entry_active: false,
    exit_active: false,
});
