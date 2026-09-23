use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const LOOKBACK_BARS: usize = 31;
const HISTORY_DAYS: usize = 20;
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy)]
struct BarPoint {
    close_ns: i64,
    close_raw: i64,
    volume_raw: i64,
}

struct UsClosePressureFade {
    bars: VecDeque<BarPoint>,
    prior_window_volumes: VecDeque<i128>,
    last_signal_day: Option<i64>,
    exit_ns: Option<i64>,
}

impl UsClosePressureFade {
    fn new() -> Self {
        Self {
            bars: VecDeque::with_capacity(LOOKBACK_BARS),
            prior_window_volumes: VecDeque::with_capacity(HISTORY_DAYS),
            last_signal_day: None,
            exit_ns: None,
        }
    }

    fn window_volume(&self, minute_ns: i64) -> Option<(i64, i128)> {
        if self.bars.len() != LOOKBACK_BARS {
            return None;
        }

        let mut iter = self.bars.iter();
        let first = iter.next()?;
        let prior_close = first.close_raw;
        let mut previous_ns = first.close_ns;
        let mut volume_raw = 0_i128;

        for (index, point) in iter.enumerate() {
            if i128::from(point.close_ns) - i128::from(previous_ns)
                != i128::from(minute_ns)
            {
                return None;
            }
            previous_ns = point.close_ns;
            // The first point is the price reference at 15:30; sum the 30
            // completed one-minute bars that follow it.
            let _ = index;
            volume_raw += i128::from(point.volume_raw);
        }

        let latest = self.bars.back()?;
        Some((prior_close, volume_raw)).filter(|_| latest.close_raw > 0)
    }

    fn median_threshold_met(&self, current_volume: i128) -> bool {
        if self.prior_window_volumes.len() != HISTORY_DAYS {
            return false;
        }
        let mut sorted: Vec<i128> = self.prior_window_volumes.iter().copied().collect();
        sorted.sort_unstable();
        let middle_sum = sorted[HISTORY_DAYS / 2 - 1] + sorted[HISTORY_DAYS / 2];
        // For an even-sized history, the median is middle_sum / 2.
        // current >= 1.25 * median is equivalent to 8*current >= 5*middle_sum.
        current_volume * 8 >= middle_sum * 5
    }

    fn record_window_volume(&mut self, volume: i128) {
        self.prior_window_volumes.push_back(volume);
        if self.prior_window_volumes.len() > HISTORY_DAYS {
            self.prior_window_volumes.pop_front();
        }
    }
}

// Days since 1970-01-01, using the proleptic Gregorian calendar.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let adjusted_year = year - i64::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_year_from_days(days: i64) -> i64 {
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
    let day_of_year = day_of_era
        - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    year
}

fn first_sunday(year: i64, month: i64) -> i64 {
    let weekday_of_first = days_from_civil(year, month, 1).rem_euclid(7);
    1 + (3 - weekday_of_first).rem_euclid(7)
}

// Convert UTC nanoseconds to New York local day and minute. US DST starts at
// 07:00 UTC on the second Sunday in March and ends at 06:00 UTC on the first
// Sunday in November.
fn new_york_day_and_minute(timestamp_ns: i64) -> (i64, i64) {
    let day_ns = TimeDelta::from_days(1).as_nanos();
    let minute_ns = TimeDelta::from_mins(1).as_nanos();
    let utc_days = timestamp_ns.div_euclid(day_ns);
    let year = civil_year_from_days(utc_days);

    let march_sunday = first_sunday(year, 3) + 7;
    let november_sunday = first_sunday(year, 11);
    let dst_start = (i128::from(days_from_civil(year, 3, march_sunday))
        * i128::from(day_ns))
        + i128::from(TimeDelta::from_hours(7).as_nanos());
    let dst_end = (i128::from(days_from_civil(year, 11, november_sunday))
        * i128::from(day_ns))
        + i128::from(TimeDelta::from_hours(6).as_nanos());
    let utc_ns = i128::from(timestamp_ns);
    let offset_ns = if utc_ns >= dst_start && utc_ns < dst_end {
        TimeDelta::from_hours(4).as_nanos()
    } else {
        TimeDelta::from_hours(5).as_nanos()
    };

    let local_ns = timestamp_ns - offset_ns;
    let local_day = local_ns.div_euclid(day_ns);
    let minute_of_day = local_ns.rem_euclid(day_ns) / minute_ns;
    (local_day, minute_of_day)
}

fn is_weekday(local_day: i64) -> bool {
    matches!(local_day.rem_euclid(7), 0 | 1 | 4 | 5 | 6)
}

impl Strategy for UsClosePressureFade {
    fn name(&self) -> String {
        "US Close Pressure Fade".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let close_ns = bar.close_ts.as_nanos();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        self.bars.push_back(BarPoint {
            close_ns,
            close_raw: bar.close.raw(),
            volume_raw: bar.volume.raw(),
        });
        if self.bars.len() > LOOKBACK_BARS {
            self.bars.pop_front();
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_zero() && self.exit_ns.is_some() {
            self.exit_ns = None;
        }

        // Submit after the bar ending at the scheduled exit time so the market
        // order is available for that bar's open under the bar callback model.
        if position.is_positive() {
            if let Some(exit_ns) = self.exit_ns {
                if close_ns >= exit_ns && ctx.open_orders() == 0 {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                }
            }
            return;
        }
        if !position.is_zero() {
            return;
        }

        let (local_day, minute_of_day) = new_york_day_and_minute(close_ns);
        if minute_of_day != 16 * 60 || !is_weekday(local_day) {
            return;
        }
        if self.last_signal_day == Some(local_day) {
            return;
        }
        self.last_signal_day = Some(local_day);

        let Some((prior_close_raw, window_volume)) = self.window_volume(minute_ns) else {
            return;
        };
        let current_close_raw = bar.close.raw();
        if prior_close_raw <= 0 {
            self.record_window_volume(window_volume);
            return;
        }

        let return_is_low = i128::from(current_close_raw) * 1_000
            <= i128::from(prior_close_raw) * 997;
        let volume_is_high = self.median_threshold_met(window_volume);

        // Include today's completed window only after comparing with the prior
        // 20 weekdays, so it cannot enter its own benchmark.
        self.record_window_volume(window_volume);

        if !return_is_low || !volume_is_high || ctx.open_orders() != 0 {
            return;
        }

        // Only use information available at the signal bar close. The next
        // bar's volume is not yet known, so cap against the latest completed
        // bar's volume rather than looking ahead.
        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        let volume_cap = bar.volume.mul_ratio(1, 10);
        let qty = Qty::from_raw(affordable.raw().min(volume_cap.raw()));
        if qty.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        match fill.side {
            Side::Buy => {
                if self.exit_ns.is_none() {
                    let hold_ns = TimeDelta::from_mins(120).as_nanos();
                    self.exit_ns = Some(fill.ts.as_nanos().saturating_add(hold_ns));
                }
            }
            Side::Sell => {
                if ctx.position(INSTRUMENT).is_zero() {
                    self.exit_ns = None;
                }
            }
        }
    }
}

export_strategy!(UsClosePressureFade::new());
