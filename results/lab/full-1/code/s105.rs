use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const HISTORY_SESSIONS: usize = 30;
const WINDOW_START_MINUTE: i64 = 15 * 60 + 45;
const WINDOW_END_MINUTE: i64 = 16 * 60;
const EXIT_MINUTE: i64 = 18 * 60;

#[derive(Clone, Copy)]
enum PendingKind {
    Entry,
    Exit,
}

struct ClosePressureFade {
    history: VecDeque<i64>,
    interval_day: Option<i64>,
    interval_baseline: Option<i64>,
    interval_volume: i128,
    interval_bars: u8,
    evaluated_day: Option<i64>,
    entry_day: Option<i64>,
    pending: Option<PendingKind>,
    remaining: Qty,
}

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
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
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

fn weekday(days: i64) -> i64 {
    (days + 4).rem_euclid(7) // Sunday = 3; Unix epoch day is Thursday = 0.
}

fn month_length(year: i64, month: i64) -> i64 {
    match month {
        4 | 6 | 9 | 11 => 30,
        2 if year % 400 == 0 || (year % 4 == 0 && year % 100 != 0) => 29,
        2 => 28,
        _ => 31,
    }
}

fn first_sunday(year: i64, month: i64) -> i64 {
    let first_weekday = weekday(days_from_civil(year, month, 1));
    1 + (3 - first_weekday).rem_euclid(7)
}

fn last_sunday(year: i64, month: i64) -> i64 {
    let last_day = month_length(year, month);
    let last_weekday = weekday(days_from_civil(year, month, last_day));
    last_day - (last_weekday - 3).rem_euclid(7)
}

fn new_york_dst_bounds(year: i64) -> Option<(i64, i64)> {
    let (start_month, start_day, end_month, end_day) = if year >= 2007 {
        (3, first_sunday(year, 3) + 7, 11, first_sunday(year, 11))
    } else if year >= 1987 {
        (4, first_sunday(year, 4), 10, last_sunday(year, 10))
    } else if year == 1974 {
        (1, 6, 10, last_sunday(year, 10))
    } else if year == 1975 {
        (2, 23, 10, last_sunday(year, 10))
    } else if year >= 1967 {
        (4, last_sunday(year, 4), 10, last_sunday(year, 10))
    } else {
        return None;
    };

    // U.S. transitions occur at 02:00 local time: 07:00 UTC on entry,
    // and 06:00 UTC on exit.
    let start_day = days_from_civil(year, start_month, start_day);
    let end_day = days_from_civil(year, end_month, end_day);
    Some((start_day * 1_440 + 7 * 60, end_day * 1_440 + 6 * 60))
}

fn new_york_time(ts_nanos: i64) -> (i64, i64) {
    let minute_nanos = TimeDelta::from_mins(1).as_nanos();
    let utc_minute = ts_nanos.div_euclid(minute_nanos);
    let utc_day = utc_minute.div_euclid(1_440);
    let (year, _, _) = civil_from_days(utc_day);
    let is_dst = match new_york_dst_bounds(year) {
        Some((start, end)) => utc_minute >= start && utc_minute < end,
        None => false,
    };
    let local_minute = utc_minute + if is_dst { -4 * 60 } else { -5 * 60 };
    (
        local_minute.div_euclid(1_440),
        local_minute.rem_euclid(1_440),
    )
}

impl ClosePressureFade {
    fn clear_pending(&mut self) {
        self.pending = None;
        self.remaining = Qty::from_raw(0);
    }

    fn submit_entry(&mut self, ctx: &mut Ctx, qty: Qty, day: i64) {
        if !qty.is_positive() || ctx.open_orders() != 0 {
            return;
        }
        self.entry_day = Some(day);
        self.pending = Some(PendingKind::Entry);
        self.remaining = qty;
        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
    }

    fn submit_exit(&mut self, ctx: &mut Ctx, qty: Qty) {
        if !qty.is_positive() || ctx.open_orders() != 0 {
            return;
        }
        self.pending = Some(PendingKind::Exit);
        self.remaining = qty;
        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
    }

    fn retry_pending(&mut self, ctx: &mut Ctx) {
        if ctx.open_orders() != 0 {
            return;
        }
        let Some(kind) = self.pending else {
            return;
        };
        match kind {
            PendingKind::Entry => {
                if self.remaining.is_positive() {
                    ctx.submit(OrderRequest::market(
                        INSTRUMENT,
                        Side::Buy,
                        self.remaining,
                    ));
                } else {
                    self.clear_pending();
                }
            }
            PendingKind::Exit => {
                let position = ctx.position(INSTRUMENT);
                let qty_raw = self.remaining.raw().min(position.raw());
                if qty_raw > 0 {
                    let qty = Qty::from_raw(qty_raw);
                    self.remaining = qty;
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
                } else {
                    self.clear_pending();
                }
            }
        }
    }

    fn update_interval(&mut self, bar: &Bar, day: i64, minute: i64) -> bool {
        if minute == WINDOW_START_MINUTE {
            self.interval_day = Some(day);
            self.interval_baseline = Some(bar.close.raw());
            self.interval_volume = 0;
            self.interval_bars = 0;
            return false;
        }

        if self.interval_day == Some(day)
            && minute > WINDOW_START_MINUTE
            && minute <= WINDOW_END_MINUTE
        {
            self.interval_volume += i128::from(bar.volume.raw());
            self.interval_bars = self.interval_bars.saturating_add(1);
        }

        if minute != WINDOW_END_MINUTE || self.evaluated_day == Some(day) {
            return false;
        }
        self.evaluated_day = Some(day);

        let complete = self.interval_day == Some(day)
            && self.interval_bars == 15
            && self.interval_baseline.is_some();
        if !complete {
            return false;
        }

        let baseline = self.interval_baseline.unwrap_or(0);
        let volume_raw = self.interval_volume.min(i128::from(i64::MAX)) as i64;
        let enough_history = self.history.len() == HISTORY_SESSIONS;
        let signal = if enough_history && baseline > 0 {
            let mut sorted: Vec<i64> = self.history.iter().copied().collect();
            sorted.sort_unstable();
            let heavy_volume = 4 * i128::from(volume_raw)
                >= 3 * (i128::from(sorted[14]) + i128::from(sorted[15]));
            let sharp_drop = 1_000 * i128::from(bar.close.raw())
                <= 996 * i128::from(baseline);
            heavy_volume && sharp_drop
        } else {
            false
        };

        self.history.push_back(volume_raw);
        if self.history.len() > HISTORY_SESSIONS {
            self.history.pop_front();
        }
        signal
    }
}

impl Strategy for ClosePressureFade {
    fn name(&self) -> String {
        "U.S. Close Pressure Fade".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let (day, minute) = new_york_time(bar.close_ts.as_nanos());
        let exit_due = match self.entry_day {
            Some(entry_day) => day > entry_day || (day == entry_day && minute >= EXIT_MINUTE),
            None => false,
        };

        if exit_due {
            if matches!(self.pending, Some(PendingKind::Entry)) {
                self.clear_pending();
            }
            if ctx.open_orders() == 0 {
                if matches!(self.pending, Some(PendingKind::Exit)) {
                    self.retry_pending(ctx);
                } else {
                    let position = ctx.position(INSTRUMENT);
                    if position.is_positive() {
                        self.submit_exit(ctx, position);
                    }
                }
            }
            return;
        }

        self.retry_pending(ctx);
        if ctx.position(INSTRUMENT).is_zero() && self.pending.is_none() {
            self.entry_day = None;
        }

        let signal = self.update_interval(bar, day, minute);
        if !signal
            || minute >= EXIT_MINUTE
            || ctx.open_orders() != 0
            || self.pending.is_some()
            || !ctx.position(INSTRUMENT).is_zero()
        {
            return;
        }

        let cash_to_use = ctx.cash().mul_ratio(95, 100);
        let qty = ctx.affordable_qty(INSTRUMENT, bar.close, cash_to_use, SIZING_MARGIN_PPM);
        self.submit_entry(ctx, qty, day);
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        let matches_pending = matches!(
            (self.pending, fill.side),
            (Some(PendingKind::Entry), Side::Buy) | (Some(PendingKind::Exit), Side::Sell)
        );
        if !matches_pending {
            return;
        }

        let remaining_raw = self.remaining.raw().saturating_sub(fill.qty.raw()).max(0);
        if remaining_raw == 0 {
            self.clear_pending();
        } else {
            self.remaining = Qty::from_raw(remaining_raw);
        }
    }
}

export_strategy!(ClosePressureFade {
    history: VecDeque::with_capacity(HISTORY_SESSIONS),
    interval_day: None,
    interval_baseline: None,
    interval_volume: 0,
    interval_bars: 0,
    evaluated_day: None,
    entry_day: None,
    pending: None,
    remaining: Qty::from_raw(0),
});
