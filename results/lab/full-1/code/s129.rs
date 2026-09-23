use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const SLIPPAGE_PPM: i64 = 200;
const WINDOW_START_MINUTE: i64 = 15 * 60;
const WINDOW_END_MINUTE: i64 = 15 * 60 + 59;
const REQUIRED_PRIOR_WINDOWS: usize = 20;
const HOLD_MINUTES: i64 = 120;

struct Window {
    active: bool,
    local_day: i64,
    first_open: Price,
    volume_raw: i128,
    bars: usize,
    last_abs_minute: i64,
}

impl Window {
    fn new() -> Self {
        Self {
            active: false,
            local_day: 0,
            first_open: Price::from_raw(0),
            volume_raw: 0,
            bars: 0,
            last_abs_minute: 0,
        }
    }
}

// Days since 1970-01-01, using the proleptic Gregorian calendar.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 {
        year / 400
    } else {
        (year - 399) / 400
    };
    let year_of_era = year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_year_from_days(days: i64) -> i64 {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted / 146_097
    } else {
        (shifted - 146_096) / 146_097
    };
    let day_of_era = shifted - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524
        - day_of_era / 146_096)
        / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_part = (5 * day_of_year + 2) / 153;
    let month = month_part + if month_part < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    year
}

fn last_sunday_of_month(year: i64, month: i64, last_day: i64) -> i64 {
    let day = days_from_civil(year, month, last_day);
    // Weekdays are numbered from Thursday = 0, so Sunday = 3.
    day - (day.rem_euclid(7) - 3).rem_euclid(7)
}

fn london_local_minute(timestamp_ns: i64) -> (i64, i64) {
    let day_ns = TimeDelta::from_days(1).as_nanos();
    let hour_ns = TimeDelta::from_hours(1).as_nanos();
    let utc_day = timestamp_ns.div_euclid(day_ns);
    let utc_ns_of_day = timestamp_ns.rem_euclid(day_ns);
    let year = civil_year_from_days(utc_day);

    // UK daylight saving starts at 01:00 UTC on the last Sunday in March
    // and ends at 01:00 UTC on the last Sunday in October.
    let dst_start_day = last_sunday_of_month(year, 3, 31);
    let dst_end_day = last_sunday_of_month(year, 10, 31);
    let after_start = utc_day > dst_start_day
        || (utc_day == dst_start_day && utc_ns_of_day >= hour_ns);
    let before_end = utc_day < dst_end_day
        || (utc_day == dst_end_day && utc_ns_of_day < hour_ns);
    let offset_minutes = if after_start && before_end { 60 } else { 0 };

    let minute_ns = TimeDelta::from_mins(1).as_nanos();
    let utc_minute = utc_day * 1_440 + utc_ns_of_day / minute_ns;
    let local_absolute_minute = utc_minute + offset_minutes;
    (
        local_absolute_minute.div_euclid(1_440),
        local_absolute_minute,
    )
}

struct LondonFixSelloffFade {
    window: Window,
    prior_volumes: VecDeque<i128>,
    entry_ts_ns: Option<i64>,
    completed_round_trips: u32,
}

impl LondonFixSelloffFade {
    fn completed_window(&mut self, bar: &Bar) -> Option<(i128, f64)> {
        let (local_day, abs_minute) = london_local_minute(bar.open_ts.as_nanos());
        let minute_of_day = abs_minute.rem_euclid(1_440);

        if minute_of_day == WINDOW_START_MINUTE {
            self.window.active = true;
            self.window.local_day = local_day;
            self.window.first_open = bar.open;
            self.window.volume_raw = i128::from(bar.volume.raw());
            self.window.bars = 1;
            self.window.last_abs_minute = abs_minute;
            return None;
        }

        if !(WINDOW_START_MINUTE + 1..=WINDOW_END_MINUTE).contains(&minute_of_day) {
            self.window.active = false;
            return None;
        }

        if !self.window.active
            || self.window.local_day != local_day
            || abs_minute != self.window.last_abs_minute + 1
        {
            self.window.active = false;
            return None;
        }

        self.window.volume_raw += i128::from(bar.volume.raw());
        self.window.bars += 1;
        self.window.last_abs_minute = abs_minute;

        if minute_of_day != WINDOW_END_MINUTE {
            return None;
        }

        self.window.active = false;
        if self.window.bars != 60 || self.window.first_open.raw() <= 0 {
            return None;
        }

        let return_fraction = bar.close.to_f64() / self.window.first_open.to_f64() - 1.0;
        Some((self.window.volume_raw, return_fraction))
    }

    fn volume_exceeds_prior_median(&self, volume: i128) -> bool {
        if self.prior_volumes.len() < REQUIRED_PRIOR_WINDOWS {
            return false;
        }
        let mut sorted: Vec<i128> = self.prior_volumes.iter().copied().collect();
        sorted.sort_unstable();
        // With 20 observations, the median is the mean of the two middle values.
        volume * 2 > sorted[9] + sorted[10]
    }

    fn remember_volume(&mut self, volume: i128) {
        self.prior_volumes.push_back(volume);
        if self.prior_volumes.len() > REQUIRED_PRIOR_WINDOWS {
            self.prior_volumes.pop_front();
        }
    }
}

impl Strategy for LondonFixSelloffFade {
    fn name(&self) -> String {
        "London Fix Selloff Fade".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        // Submit at the close immediately before the scheduled exit open. With
        // one-minute bars, this is the bar whose close is the target timestamp.
        if let Some(entry_ns) = self.entry_ts_ns {
            let exit_ns = entry_ns.saturating_add(TimeDelta::from_mins(HOLD_MINUTES).as_nanos());
            if bar.close_ts.as_nanos() >= exit_ns
                && ctx.position(INSTRUMENT).is_positive()
                && ctx.open_orders() == 0
            {
                let position = ctx.position(INSTRUMENT);
                let volume_cap = bar.volume.mul_ratio(1, 10);
                let qty = if position.raw() < volume_cap.raw() {
                    position
                } else {
                    volume_cap
                };
                if qty.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
                }
            }
        }

        let Some((volume, return_fraction)) = self.completed_window(bar) else {
            return;
        };

        let signal = return_fraction <= -0.005
            && self.volume_exceeds_prior_median(volume)
            && ctx.position(INSTRUMENT).is_zero()
            && ctx.open_orders() == 0;

        if signal && bar.close.is_positive() {
            // Budget for the requested 2 bp adverse buy-side slippage when sizing.
            let sizing_price = bar.close.mul_ratio(1_000_000 + SLIPPAGE_PPM, 1_000_000);
            let cash_qty = ctx.affordable_qty(
                INSTRUMENT,
                sizing_price,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            // Only the just-closed bar's volume is observable at submission time.
            let volume_cap = bar.volume.mul_ratio(1, 10);
            let qty = if cash_qty.raw() < volume_cap.raw() {
                cash_qty
            } else {
                volume_cap
            };
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        }

        self.remember_volume(volume);
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.side == Side::Buy {
            if self.entry_ts_ns.is_none() {
                self.entry_ts_ns = Some(fill.ts.as_nanos());
            }
        } else if fill.side == Side::Sell && ctx.position(INSTRUMENT).is_zero() {
            self.entry_ts_ns = None;
            self.completed_round_trips = self.completed_round_trips.saturating_add(1);
        }
    }

    fn on_finish(&mut self, ctx: &mut Ctx) {
        if self.completed_round_trips < 30 {
            ctx.log("London Fix Selloff Fade: fewer than 30 completed round trips; results do not meet the evaluation minimum.");
        }
    }
}

export_strategy!(LondonFixSelloffFade {
    window: Window::new(),
    prior_volumes: VecDeque::with_capacity(REQUIRED_PRIOR_WINDOWS),
    entry_ts_ns: None,
    completed_round_trips: 0,
});
