use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, Fill, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, TimeInForce,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const HISTORY_WEEKS: i64 = 8;
const MARGIN_PPM: i64 = 5_000;

struct WeekVolume {
    week: i64,
    volume_raw: i64,
}

struct ClaimsContinuation {
    anchor: Option<(i64, Price)>,
    window_week: Option<i64>,
    window_volume_raw: i128,
    window_bars: u8,
    window_close: Option<Price>,
    history: VecDeque<WeekVolume>,
    processed_week: Option<i64>,
    traded_week: Option<i64>,
    entry_ts_nanos: Option<i64>,
    trade_anchor: Option<Price>,
    exit_requested: bool,
}

impl ClaimsContinuation {
    fn new() -> Self {
        Self {
            anchor: None,
            window_week: None,
            window_volume_raw: 0,
            window_bars: 0,
            window_close: None,
            history: VecDeque::new(),
            processed_week: None,
            traded_week: None,
            entry_ts_nanos: None,
            trade_anchor: None,
            exit_requested: false,
        }
    }

    fn historical_median(&mut self, week: i64) -> Option<i64> {
        let first_week = week.saturating_sub(HISTORY_WEEKS);
        while self
            .history
            .front()
            .is_some_and(|item| item.week < first_week)
        {
            self.history.pop_front();
        }

        let mut values: Vec<i64> = self
            .history
            .iter()
            .filter(|item| item.week >= first_week && item.week < week)
            .map(|item| item.volume_raw)
            .collect();
        // Require observations from all eight preceding weekly windows.
        if values.len() != HISTORY_WEEKS as usize {
            return None;
        }
        values.sort_unstable();
        let middle = values.len() / 2;
        Some(((i128::from(values[middle - 1]) + i128::from(values[middle])) / 2) as i64)
    }

    fn submit_exit(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if !self.exit_requested || ctx.open_orders() != 0 {
            return;
        }
        let position = ctx.position(INSTRUMENT);
        if !position.is_positive() {
            return;
        }

        // IOC orders cancel any remainder; retry a later bar if still in position.
        let cap = bar.volume.mul_ratio(1, 10);
        let qty = if position < cap { position } else { cap };
        if qty.is_positive() {
            ctx.submit(
                OrderRequest::market(INSTRUMENT, Side::Sell, qty)
                    .with_time_in_force(TimeInForce::Ioc),
            );
        }
    }
}

impl Strategy for ClaimsContinuation {
    fn name(&self) -> String {
        "claims-release-continuation".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let utc_minute = bar
            .close_ts
            .as_nanos()
            .div_euclid(TimeDelta::from_mins(1).as_nanos());
        let (local_minute, local_day) = new_york_time(utc_minute);
        let weekday = local_day.rem_euclid(7); // Thursday is day zero from the Unix epoch.
        let week = local_day.div_euclid(7);
        let minute_of_day = local_minute.rem_euclid(1440);

        if weekday == 0 {
            if minute_of_day == 8 * 60 + 29 {
                self.anchor = Some((week, bar.close));
            } else if minute_of_day == 8 * 60 + 30 {
                self.window_week = Some(week);
                self.window_volume_raw = i128::from(bar.volume.raw().max(0));
                self.window_bars = 1;
                self.window_close = None;
            } else if minute_of_day >= 8 * 60 + 31 && minute_of_day <= 8 * 60 + 34 {
                if self.window_week == Some(week) {
                    self.window_volume_raw += i128::from(bar.volume.raw().max(0));
                    self.window_bars = self.window_bars.saturating_add(1);
                    if minute_of_day == 8 * 60 + 34 {
                        self.window_close = Some(bar.close);
                    }
                }
            } else if minute_of_day == 8 * 60 + 35 && self.processed_week != Some(week) {
                self.processed_week = Some(week);
                let window_complete = self.window_week == Some(week) && self.window_bars == 5;

                if window_complete {
                    let current_volume = self.window_volume_raw;
                    let median = self.historical_median(week);
                    let anchor = self
                        .anchor
                        .filter(|(anchor_week, _)| *anchor_week == week)
                        .map(|(_, price)| price);

                    if let (Some(median_raw), Some(anchor_price), Some(signal_close)) =
                        (median, anchor, self.window_close)
                    {
                        let return_is_large_enough = anchor_price.raw() > 0
                            && i128::from(signal_close.raw()) * 10_000
                                >= i128::from(anchor_price.raw()) * 10_025;
                        let volume_is_large_enough = current_volume >= i128::from(median_raw);

                        if return_is_large_enough
                            && volume_is_large_enough
                            && ctx.position(INSTRUMENT).is_zero()
                            && ctx.open_orders() == 0
                            && self.traded_week != Some(week)
                        {
                            let affordable = ctx.affordable_qty(
                                INSTRUMENT,
                                bar.close,
                                ctx.cash(),
                                MARGIN_PPM,
                            );
                            // The next bar's volume is not yet observable here. Use the
                            // latest completed bar as the available participation cap.
                            let cap = bar.volume.mul_ratio(1, 10);
                            let qty = if affordable < cap { affordable } else { cap };
                            if qty.is_positive() {
                                self.traded_week = Some(week);
                                self.trade_anchor = Some(anchor_price);
                                ctx.submit(
                                    OrderRequest::market(INSTRUMENT, Side::Buy, qty)
                                        .with_time_in_force(TimeInForce::Ioc),
                                );
                            }
                        }
                    }

                    let volume_raw = current_volume.clamp(0, i128::from(i64::MAX)) as i64;
                    self.history.push_back(WeekVolume { week, volume_raw });
                }
            }
        }

        if let Some(entry_ts) = self.entry_ts_nanos {
            let is_30_minute_close = minute_of_day % 30 == 0;
            let below_release_anchor = is_30_minute_close
                && self
                    .trade_anchor
                    .is_some_and(|anchor| bar.close < anchor);
            let four_hours_elapsed = bar.close_ts.as_nanos()
                >= entry_ts.saturating_add(TimeDelta::from_hours(4).as_nanos());
            if below_release_anchor || four_hours_elapsed {
                self.exit_requested = true;
            }
        }
        self.submit_exit(ctx, bar);
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &Fill) {
        match fill.side {
            Side::Buy => {
                if self.entry_ts_nanos.is_none() {
                    self.entry_ts_nanos = Some(fill.ts.as_nanos());
                }
            }
            Side::Sell => {
                if ctx.position(INSTRUMENT).is_zero() {
                    self.entry_ts_nanos = None;
                    self.trade_anchor = None;
                    self.exit_requested = false;
                }
            }
        }
    }
}

// Convert UTC epoch minutes to New York local time using current U.S. DST rules.
fn new_york_time(utc_minute: i64) -> (i64, i64) {
    let utc_day = utc_minute.div_euclid(1440);
    let (year, _, _) = civil_from_days(utc_day);
    let march_sunday = nth_sunday_of_month(year, 3, 2);
    let november_sunday = nth_sunday_of_month(year, 11, 1);
    let dst_start = days_from_civil(year, 3, march_sunday) * 1440 + 7 * 60;
    let dst_end = days_from_civil(year, 11, november_sunday) * 1440 + 6 * 60;
    let offset_minutes = if utc_minute >= dst_start && utc_minute < dst_end {
        -4 * 60
    } else {
        -5 * 60
    };
    let local_minute = utc_minute + offset_minutes;
    (local_minute, local_minute.div_euclid(1440))
}

fn nth_sunday_of_month(year: i64, month: i64, occurrence: i64) -> i64 {
    let first_day = days_from_civil(year, month, 1);
    let first_weekday = first_day.rem_euclid(7);
    let days_until_sunday = (3 - first_weekday).rem_euclid(7);
    1 + days_until_sunday + (occurrence - 1) * 7
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
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

export_strategy!(ClaimsContinuation::new());
