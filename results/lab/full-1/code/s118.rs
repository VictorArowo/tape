use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const LOOKBACK_WEEKDAYS: usize = 20;
const SIZING_MARGIN_PPM: i64 = 5_000;
const OPEN_WINDOW_START: i64 = 9 * 60 + 30;
const OPEN_WINDOW_END: i64 = 9 * 60 + 44;
const EXIT_ORDER_MINUTE: i64 = 11 * 60 + 59;
const WINDOW_MASK: u16 = (1u16 << 15) - 1;

struct LocalTime {
    day: i64,
    weekday_sun0: i64,
    minute: i64,
}

fn weekday_sun0(day: i64) -> i64 {
    (day + 4).rem_euclid(7)
}

fn days_from_civil(mut year: i64, month: i64, day: i64) -> i64 {
    year -= i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524
        - day_of_era / 146_096)
        / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn first_sunday_of_month(year: i64, month: i64) -> i64 {
    let first_day = days_from_civil(year, month, 1);
    1 + (7 - weekday_sun0(first_day)).rem_euclid(7)
}

fn new_york_time(ts: tape_guest::Timestamp) -> LocalTime {
    let minute_ns = TimeDelta::from_mins(1).as_nanos();
    let utc_minute = ts.as_nanos().div_euclid(minute_ns);
    let utc_day = utc_minute.div_euclid(1_440);
    let (year, _, _) = civil_from_days(utc_day);

    // Current U.S. daylight-saving rules: second Sunday in March through
    // first Sunday in November, with transitions at 02:00 local time.
    let march_sunday = first_sunday_of_month(year, 3) + 7;
    let november_sunday = first_sunday_of_month(year, 11);
    let dst_start_day = days_from_civil(year, 3, march_sunday);
    let dst_end_day = days_from_civil(year, 11, november_sunday);
    let dst_start_utc = dst_start_day * 1_440 + 7 * 60;
    let dst_end_utc = dst_end_day * 1_440 + 6 * 60;

    let offset_minutes = if utc_minute >= dst_start_utc && utc_minute < dst_end_utc {
        4 * 60
    } else {
        5 * 60
    };
    let local_minute = utc_minute - offset_minutes;
    let local_day = local_minute.div_euclid(1_440);

    LocalTime {
        day: local_day,
        weekday_sun0: weekday_sun0(local_day),
        minute: local_minute.rem_euclid(1_440),
    }
}

struct UsOpenFlowDrift {
    active_day: Option<i64>,
    window_mask: u16,
    window_open_raw: Option<i64>,
    window_close_raw: Option<i64>,
    window_volume_raw: i128,
    weekday_window_volumes: VecDeque<i64>,
}

impl UsOpenFlowDrift {
    fn reset_window(&mut self, day: i64) {
        self.active_day = Some(day);
        self.window_mask = 0;
        self.window_open_raw = None;
        self.window_close_raw = None;
        self.window_volume_raw = 0;
    }

    fn volume_is_at_least_one_and_a_half_median(&self, volume: i64) -> bool {
        if self.weekday_window_volumes.len() != LOOKBACK_WEEKDAYS {
            return false;
        }
        let mut sorted: Vec<i64> = self.weekday_window_volumes.iter().copied().collect();
        sorted.sort_unstable();
        let middle_sum = i128::from(sorted[9]) + i128::from(sorted[10]);
        i128::from(volume) * 4 >= middle_sum * 3
    }

    fn push_weekday_volume(&mut self, volume: i64) {
        if self.weekday_window_volumes.len() == LOOKBACK_WEEKDAYS {
            self.weekday_window_volumes.pop_front();
        }
        self.weekday_window_volumes.push_back(volume);
    }
}

impl Strategy for UsOpenFlowDrift {
    fn name(&self) -> String {
        "U.S. Open Flow Drift".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let local = new_york_time(bar.open_ts);
        if self.active_day != Some(local.day) {
            self.reset_window(local.day);
        }

        let weekday = (1..=5).contains(&local.weekday_sun0);
        if weekday && (OPEN_WINDOW_START..=OPEN_WINDOW_END).contains(&local.minute) {
            let index = (local.minute - OPEN_WINDOW_START) as u32;
            let bit = 1u16 << index;
            if self.window_mask & bit == 0 {
                self.window_mask |= bit;
                self.window_volume_raw += i128::from(bar.volume.raw());
                if local.minute == OPEN_WINDOW_START {
                    self.window_open_raw = Some(bar.open.raw());
                }
                if local.minute == OPEN_WINDOW_END {
                    self.window_close_raw = Some(bar.close.raw());
                }
            }

            if local.minute == OPEN_WINDOW_END && self.window_mask == WINDOW_MASK {
                let volume = self
                    .window_volume_raw
                    .clamp(0, i128::from(i64::MAX)) as i64;
                let return_is_strong = match (self.window_open_raw, self.window_close_raw) {
                    (Some(open), Some(close)) if open > 0 => {
                        i128::from(close) * 1_000 > i128::from(open) * 1_004
                    }
                    _ => false,
                };
                let volume_is_unusual = self.volume_is_at_least_one_and_a_half_median(volume);

                if return_is_strong
                    && volume_is_unusual
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
                        // Submitted after the 09:44 bar closes; the next bar opens at 09:45.
                        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                    }
                }

                self.push_weekday_volume(volume);
            }
        }

        if weekday && local.minute == EXIT_ORDER_MINUTE && ctx.open_orders() == 0 {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                // Submitted after the 11:59 bar closes; the next bar opens at 12:00.
                // Use actual held inventory, so a partial entry is never over-exited.
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
        }
    }
}

export_strategy!(UsOpenFlowDrift {
    active_day: None,
    window_mask: 0,
    window_open_raw: None,
    window_close_raw: None,
    window_volume_raw: 0,
    weekday_window_volumes: VecDeque::with_capacity(LOOKBACK_WEEKDAYS),
});
