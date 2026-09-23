use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const WINDOW_START_MINUTE: i64 = 17 * 60;
const WINDOW_BARS: usize = 15;
const HISTORY_WEEKS: usize = 8;
const HOLD_MINUTES: i64 = 240;

struct CmeReopenFollowThrough {
    window_day: Option<i64>,
    window_count: usize,
    window_open: Option<Price>,
    window_volume: i128,
    prior_volumes: VecDeque<i128>,
    entry_ts_ns: Option<i64>,
    liquidating: bool,
}

impl CmeReopenFollowThrough {
    fn clear_window(&mut self) {
        self.window_day = None;
        self.window_count = 0;
        self.window_open = None;
        self.window_volume = 0;
    }

    fn update_window(&mut self, day: i64, minute: i64, bar: &Bar) -> Option<(Price, Price, i128)> {
        if minute == WINDOW_START_MINUTE && day.rem_euclid(7) == 3 {
            self.window_day = Some(day);
            self.window_count = 1;
            self.window_open = Some(bar.open);
            self.window_volume = i128::from(bar.volume.raw());
            return None;
        }

        let expected_minute = WINDOW_START_MINUTE + self.window_count as i64;
        if self.window_day == Some(day)
            && self.window_count < WINDOW_BARS
            && minute == expected_minute
        {
            self.window_count += 1;
            self.window_volume += i128::from(bar.volume.raw());

            if self.window_count == WINDOW_BARS {
                let completed = self
                    .window_open
                    .take()
                    .map(|open| (open, bar.close, self.window_volume));
                self.clear_window();
                return completed;
            }
            return None;
        }

        self.clear_window();
        None
    }

    fn record_and_test_volume(&mut self, volume: i128) -> bool {
        let unusually_active = if self.prior_volumes.len() == HISTORY_WEEKS {
            let mut sorted: Vec<i128> = self.prior_volumes.iter().copied().collect();
            sorted.sort_unstable();
            // For eight observations, the median is the mean of the middle pair.
            // Compare exactly against 1.25 times that median.
            volume * 8 >= (sorted[3] + sorted[4]) * 5
        } else {
            false
        };

        self.prior_volumes.push_back(volume);
        if self.prior_volumes.len() > HISTORY_WEEKS {
            self.prior_volumes.pop_front();
        }
        unusually_active
    }

    fn submit_entry(&self, ctx: &mut Ctx, close: Price) {
        // Estimate the buy price including the stated 0.02% slippage. The simulator
        // applies actual slippage and fees to fills; affordable_qty accounts for fees.
        let slipped_raw = i128::from(close.raw()) * 10_002 / 10_000;
        let sizing_price = i64::try_from(slipped_raw)
            .map(Price::from_raw)
            .unwrap_or(close);
        let qty = ctx.affordable_qty(
            INSTRUMENT,
            sizing_price,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        if qty.is_positive() && ctx.open_orders() == 0 {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }
}

impl Strategy for CmeReopenFollowThrough {
    fn name(&self) -> String {
        "CME Reopen Follow-Through".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let (local_day, local_minute) = chicago_day_and_minute(&bar.close_ts);
        let completed_window = self.update_window(local_day, local_minute, bar);
        let mut entry_signal = false;

        if let Some((open, close, volume)) = completed_window {
            let active = self.record_and_test_volume(volume);
            let strong_move = open.is_positive()
                && i128::from(close.raw()) * 10_000
                    >= i128::from(open.raw()) * 10_015;
            entry_signal = active && strong_move;
        }

        let position = ctx.position(INSTRUMENT);

        if position.is_zero() && ctx.open_orders() == 0 {
            self.entry_ts_ns = None;
            self.liquidating = false;
        }

        if position.is_positive() {
            if let Some(entry_ts_ns) = self.entry_ts_ns {
                let hold_ns = TimeDelta::from_mins(HOLD_MINUTES).as_nanos();
                if i128::from(bar.close_ts.as_nanos()) - i128::from(entry_ts_ns)
                    >= i128::from(hold_ns)
                {
                    self.liquidating = true;
                }
            }

            if self.liquidating && ctx.open_orders() == 0 {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
            return;
        }

        if !position.is_zero() || self.liquidating || ctx.open_orders() != 0 {
            return;
        }

        if entry_signal && ctx.open_orders() == 0 {
            self.submit_entry(ctx, bar.close);
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        match fill.side {
            Side::Buy => {
                if self.entry_ts_ns.is_none() {
                    self.entry_ts_ns = Some(fill.ts.as_nanos());
                }
                self.liquidating = false;
            }
            Side::Sell => {
                if ctx.position(INSTRUMENT).is_zero() {
                    self.entry_ts_ns = None;
                    // Keep the liquidation lock until the order is finished and a bar
                    // confirms there are no remaining open orders.
                    self.liquidating = true;
                }
            }
        }
    }
}

fn chicago_day_and_minute(ts: &tape_guest::Timestamp) -> (i64, i64) {
    let minute_ns = TimeDelta::from_mins(1).as_nanos();
    let utc_minute = ts.as_nanos().div_euclid(minute_ns);
    let utc_day = utc_minute.div_euclid(1_440);
    let offset_minutes = if chicago_is_dst(ts) { -5 * 60 } else { -6 * 60 };
    let local_minute = utc_minute + offset_minutes;
    (local_minute.div_euclid(1_440), local_minute.rem_euclid(1_440))
}

// America/Chicago's US daylight-saving rules, including the main rule changes
// since 1967 and the 1974/1975 emergency transitions.
fn chicago_is_dst(ts: &tape_guest::Timestamp) -> bool {
    let minute_ns = TimeDelta::from_mins(1).as_nanos();
    let utc_minute = ts.as_nanos().div_euclid(minute_ns);
    let utc_day = utc_minute.div_euclid(1_440);
    let (year, _, _) = civil_from_days(utc_day);
    if year < 1967 {
        return false;
    }

    let (start_month, start_day, end_month, end_day) = if year >= 2007 {
        (3, nth_sunday(year, 3, 2), 11, nth_sunday(year, 11, 1))
    } else if year >= 1987 {
        (4, nth_sunday(year, 4, 1), 10, last_sunday(year, 10))
    } else if year == 1975 {
        (2, 23, 10, last_sunday(year, 10))
    } else if year == 1974 {
        (1, 6, 10, last_sunday(year, 10))
    } else {
        (4, last_sunday(year, 4), 10, last_sunday(year, 10))
    };

    let start_utc_minute = days_from_civil(year, start_month, start_day) * 1_440 + 8 * 60;
    let end_utc_minute = days_from_civil(year, end_month, end_day) * 1_440 + 7 * 60;
    utc_minute >= start_utc_minute && utc_minute < end_utc_minute
}

fn nth_sunday(year: i64, month: u32, nth: i64) -> u32 {
    let first_weekday = days_from_civil(year, month, 1).rem_euclid(7);
    (1 + (3 - first_weekday).rem_euclid(7) + (nth - 1) * 7) as u32
}

fn last_sunday(year: i64, month: u32) -> u32 {
    let last_day = days_in_month(year, month) as i64;
    let weekday = days_from_civil(year, month, last_day as u32).rem_euclid(7);
    (last_day - (weekday - 3).rem_euclid(7)) as u32
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 30,
    }
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let adjusted_year = year - i64::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let adjusted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524
        - day_of_era / 146_096)
        / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month as u32, day as u32)
}

export_strategy!(CmeReopenFollowThrough {
    window_day: None,
    window_count: 0,
    window_open: None,
    window_volume: 0,
    prior_volumes: VecDeque::with_capacity(HISTORY_WEEKS),
    entry_ts_ns: None,
    liquidating: false,
});
