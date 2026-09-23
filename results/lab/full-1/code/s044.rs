use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, Fill, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const HISTORY_DAYS: usize = 20;

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let adjusted_year = year - if month <= 2 { 1 } else { 0 };
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let shifted_days = days_since_epoch + 719_468;
    let era = shifted_days.div_euclid(146_097);
    let day_of_era = shifted_days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_part = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = month_part + if month_part < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

fn last_sunday_of_month(year: i64, month: i64) -> i64 {
    let first_of_next_month = days_from_civil(year, month + 1, 1);
    let last_day = first_of_next_month - 1;
    let weekday = last_day.rem_euclid(7); // 0 = Thursday, 3 = Sunday
    last_day - (weekday - 3).rem_euclid(7)
}

fn london_day_and_minute(timestamp_ns: i64) -> (i64, i64) {
    let day_ns = TimeDelta::from_days(1).as_nanos();
    let minute_ns = TimeDelta::from_mins(1).as_nanos();
    let utc_day = timestamp_ns.div_euclid(day_ns);
    let within_utc_day = timestamp_ns.rem_euclid(day_ns);
    let (year, _, _) = civil_from_days(utc_day);

    // Europe/London changes clocks at 01:00 UTC on the last Sundays of March and October.
    let spring_day = last_sunday_of_month(year, 3);
    let autumn_day = last_sunday_of_month(year, 10);
    let one_hour_ns = TimeDelta::from_hours(1).as_nanos();
    let is_summer_time = (utc_day > spring_day && utc_day < autumn_day)
        || (utc_day == spring_day && within_utc_day >= one_hour_ns)
        || (utc_day == autumn_day && within_utc_day < one_hour_ns);
    let offset_hours = if is_summer_time { 2 } else { 1 };
    let local_ns = timestamp_ns + TimeDelta::from_hours(offset_hours).as_nanos();

    (
        local_ns.div_euclid(day_ns),
        local_ns.div_euclid(minute_ns).rem_euclid(1_440),
    )
}

struct LondonFixingFade {
    prior_volumes: VecDeque<i128>,
    current_day: Option<i64>,
    window_open_raw: Option<i64>,
    window_volume_raw: i128,
    window_count: usize,
    window_valid: bool,
    signal_processed: bool,
    last_entry_day: Option<i64>,
    exit_at_ns: Option<i64>,
}

impl LondonFixingFade {
    fn volume_is_unusually_high(&self, current_volume: i128) -> bool {
        if self.prior_volumes.len() != HISTORY_DAYS {
            return false;
        }
        let mut sorted: Vec<i128> = self.prior_volumes.iter().copied().collect();
        sorted.sort_unstable();

        // For 20 observations, the median is the mean of the two middle values.
        // Compare without rounding: current >= 1.2 * median.
        5 * current_volume >= 3 * (sorted[9] + sorted[10])
    }

    fn remember_volume(&mut self, volume_raw: i128) {
        self.prior_volumes.push_back(volume_raw);
        if self.prior_volumes.len() > HISTORY_DAYS {
            self.prior_volumes.pop_front();
        }
    }
}

impl Strategy for LondonFixingFade {
    fn name(&self) -> String {
        "London Fixing Fade".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let (local_day, local_minute) = london_day_and_minute(bar.open_ts.as_nanos());
        if self.current_day != Some(local_day) {
            self.current_day = Some(local_day);
            self.window_open_raw = None;
            self.window_volume_raw = 0;
            self.window_count = 0;
            self.window_valid = false;
            self.signal_processed = false;
        }

        // Accumulate the completed bars beginning from 15:00 through 15:59 local time.
        if (900..960).contains(&local_minute) {
            if local_minute == 900 {
                self.window_open_raw = Some(bar.open.raw());
                self.window_volume_raw = 0;
                self.window_count = 0;
                self.window_valid = true;
            }

            let expected_count = (local_minute - 900) as usize;
            if self.window_valid && self.window_open_raw.is_some()
                && self.window_count == expected_count
            {
                self.window_volume_raw += i128::from(bar.volume.raw());
                self.window_count += 1;
            } else {
                self.window_valid = false;
            }
        }

        let mut entry_signal = false;
        // The bar starting at 16:00 closes at 16:01; its close completes the fixing return.
        if local_minute == 960 && !self.signal_processed {
            self.signal_processed = true;
            let complete_window = self.window_valid
                && self.window_count == 60
                && self.window_open_raw.is_some();

            if complete_window {
                let start_raw = i128::from(self.window_open_raw.expect("checked above"));
                let close_raw = i128::from(bar.close.raw());
                let return_is_low = start_raw > 0 && close_raw * 10_000 <= start_raw * 9_965;
                let volume_is_high = self.volume_is_unusually_high(self.window_volume_raw);
                entry_signal = return_is_low && volume_is_high;
                self.remember_volume(self.window_volume_raw);
            }
        }

        // Submit on the bar that closes at the scheduled exit time, so the order can
        // execute at that next bar's open. Retry on a later bar if the order cannot fill.
        if let Some(exit_at_ns) = self.exit_at_ns {
            if bar.close_ts.as_nanos() >= exit_at_ns
                && ctx.position(INSTRUMENT).is_positive()
                && ctx.open_orders() == 0
            {
                let position = ctx.position(INSTRUMENT);
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                return;
            }
        }

        if entry_signal
            && ctx.position(INSTRUMENT).is_zero()
            && self.last_entry_day != Some(local_day)
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
                self.last_entry_day = Some(local_day);
            }
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &Fill) {
        match fill.side {
            Side::Buy => {
                if self.exit_at_ns.is_none() {
                    self.exit_at_ns = Some(
                        fill.ts
                            .as_nanos()
                            .saturating_add(TimeDelta::from_mins(120).as_nanos()),
                    );
                }
            }
            Side::Sell => {
                if ctx.position(INSTRUMENT).is_zero() {
                    self.exit_at_ns = None;
                }
            }
        }
    }
}

export_strategy!(LondonFixingFade {
    prior_volumes: VecDeque::with_capacity(HISTORY_DAYS + 1),
    current_day: None,
    window_open_raw: None,
    window_volume_raw: 0,
    window_count: 0,
    window_valid: false,
    signal_processed: false,
    last_entry_day: None,
    exit_at_ns: None,
});
