use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SESSION_START: i32 = 9 * 60 + 30;
const SESSION_END: i32 = 15 * 60 + 59;
const SESSION_BARS: u16 = 390;
const LOOKBACK_SESSIONS: usize = 20;
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy)]
struct SessionStats {
    start_open: i64,
    close: i64,
    high: i64,
    low: i64,
    volume: i128,
}

struct SessionBuilder {
    day: i64,
    expected_minute: i32,
    count: u16,
    valid: bool,
    start_open: i64,
    close: i64,
    high: i64,
    low: i64,
    volume: i128,
}

impl SessionBuilder {
    fn new(day: i64, bar: &Bar) -> Self {
        Self {
            day,
            expected_minute: SESSION_START + 1,
            count: 1,
            valid: true,
            start_open: bar.open.raw(),
            close: bar.close.raw(),
            high: bar.high.raw(),
            low: bar.low.raw(),
            volume: i128::from(bar.volume.raw()),
        }
    }

    fn push(&mut self, minute: i32, bar: &Bar) {
        if minute != self.expected_minute {
            self.valid = false;
        }
        self.expected_minute = minute + 1;
        self.count = self.count.saturating_add(1);
        self.close = bar.close.raw();
        self.high = self.high.max(bar.high.raw());
        self.low = self.low.min(bar.low.raw());
        self.volume += i128::from(bar.volume.raw());
    }

    fn complete(&self) -> bool {
        self.valid && self.count == SESSION_BARS && self.expected_minute == SESSION_END + 1
    }

    fn stats(&self) -> SessionStats {
        SessionStats {
            start_open: self.start_open,
            close: self.close,
            high: self.high,
            low: self.low,
            volume: self.volume,
        }
    }
}

struct UsCashFlowContinuation {
    session: Option<SessionBuilder>,
    previous_volumes: VecDeque<i128>,
    entry_pending_day: Option<i64>,
    holding_entry_day: Option<i64>,
    exit_due: bool,
    completed_round_trips: u32,
}

impl UsCashFlowContinuation {
    fn new() -> Self {
        Self {
            session: None,
            previous_volumes: VecDeque::with_capacity(LOOKBACK_SESSIONS),
            entry_pending_day: None,
            holding_entry_day: None,
            exit_due: false,
            completed_round_trips: 0,
        }
    }

    fn sync_position(&mut self, ctx: &Ctx) {
        if self.entry_pending_day.is_some() && ctx.open_orders() == 0 {
            let signal_day = self.entry_pending_day.take();
            if ctx.position(INSTRUMENT).is_positive() {
                self.holding_entry_day = signal_day;
            }
        }

        if self.holding_entry_day.is_some()
            && ctx.position(INSTRUMENT).is_zero()
            && ctx.open_orders() == 0
        {
            self.holding_entry_day = None;
            self.exit_due = false;
        }
    }

    fn try_exit(&mut self, ctx: &mut Ctx) {
        if self.exit_due && ctx.open_orders() == 0 {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            } else {
                self.holding_entry_day = None;
                self.exit_due = false;
            }
        }
    }

    fn on_complete_session(&mut self, ctx: &mut Ctx, bar: &Bar, day: i64, stats: SessionStats) {
        let was_holding = self.holding_entry_day.is_some()
            || self.entry_pending_day.is_some()
            || self.exit_due
            || ctx.position(INSTRUMENT).is_positive();

        if let Some(entry_day) = self.holding_entry_day {
            if day > entry_day {
                self.exit_due = true;
            }
        }

        let median_volume = if self.previous_volumes.len() == LOOKBACK_SESSIONS {
            let mut ordered: Vec<i128> = self.previous_volumes.iter().copied().collect();
            ordered.sort_unstable();
            Some((ordered[9] + ordered[10]) / 2)
        } else {
            None
        };

        if !was_holding
            && ctx.position(INSTRUMENT).is_zero()
            && ctx.open_orders() == 0
            && self.entry_pending_day.is_none()
            && !self.exit_due
        {
            if let Some(median) = median_volume {
                let positive_return = stats.start_open > 0
                    && i128::from(stats.close - stats.start_open) * 250
                        >= i128::from(stats.start_open);
                let range = i128::from(stats.high) - i128::from(stats.low);
                let strong_close = range > 0
                    && (i128::from(stats.close) - i128::from(stats.low)) * 4 >= range * 3;
                let high_volume = stats.volume * 100 >= median * 115;

                if positive_return && strong_close && high_volume {
                    let qty = ctx.affordable_qty(
                        INSTRUMENT,
                        bar.close,
                        ctx.cash(),
                        SIZING_MARGIN_PPM,
                    );
                    if qty.is_positive() {
                        self.entry_pending_day = Some(day);
                        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                    }
                }
            }
        }

        self.previous_volumes.push_back(stats.volume);
        if self.previous_volumes.len() > LOOKBACK_SESSIONS {
            self.previous_volumes.pop_front();
        }
    }
}

impl Strategy for UsCashFlowContinuation {
    fn name(&self) -> String {
        "U.S. Cash-Flow Continuation".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        self.sync_position(ctx);
        self.try_exit(ctx);

        // Bar open timestamps identify the 09:30 through 15:59 session bars.
        let (day, minute) = new_york_day_and_minute(bar.open_ts.as_nanos());
        let weekday = weekday_sunday_zero(day);

        if self.session.as_ref().is_some_and(|session| session.day != day) {
            self.session = None;
        }

        let mut finished = None;
        if weekday != 0 && weekday != 6 && (SESSION_START..=SESSION_END).contains(&minute) {
            if minute == SESSION_START {
                self.session = Some(SessionBuilder::new(day, bar));
            } else if let Some(session) = self.session.as_mut() {
                session.push(minute, bar);
            }

            if minute == SESSION_END {
                if let Some(session) = self.session.take() {
                    if session.complete() {
                        finished = Some(session.stats());
                    }
                }
            }
        }

        if let Some(stats) = finished {
            self.on_complete_session(ctx, bar, day, stats);
            self.try_exit(ctx);
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        match fill.side {
            Side::Buy => {
                if self.holding_entry_day.is_none() {
                    self.holding_entry_day = self.entry_pending_day.take();
                }
            }
            Side::Sell => {
                if ctx.position(INSTRUMENT).is_zero() {
                    if self.holding_entry_day.take().is_some() {
                        self.completed_round_trips = self.completed_round_trips.saturating_add(1);
                    }
                    self.exit_due = false;
                }
            }
        }
    }

    fn on_finish(&mut self, ctx: &mut Ctx) {
        if self.completed_round_trips < 30 {
            ctx.log(&format!(
                "inconclusive: only {} completed round trips (30 required)",
                self.completed_round_trips
            ));
        } else {
            ctx.log(&format!(
                "completed round trips: {}",
                self.completed_round_trips
            ));
        }
    }
}

fn new_york_day_and_minute(utc_ns: i64) -> (i64, i32) {
    let day_ns = TimeDelta::from_days(1).as_nanos();
    let hour_ns = TimeDelta::from_hours(1).as_nanos();
    let utc_day = utc_ns.div_euclid(day_ns);
    let within_day = utc_ns.rem_euclid(day_ns);
    let (year, _, _) = civil_from_days(utc_day);

    // Modern America/New_York DST rules: second Sunday in March to first Sunday in November.
    let march_first = days_from_civil(year, 3, 1);
    let march_first_weekday = weekday_sunday_zero(march_first);
    let first_march_sunday = 1 + (7 - march_first_weekday) % 7;
    let dst_start_day = days_from_civil(year, 3, first_march_sunday + 7);

    let november_first = days_from_civil(year, 11, 1);
    let november_first_weekday = weekday_sunday_zero(november_first);
    let first_november_sunday = 1 + (7 - november_first_weekday) % 7;
    let dst_end_day = days_from_civil(year, 11, first_november_sunday);

    let after_start = utc_day > dst_start_day
        || (utc_day == dst_start_day && within_day >= 7 * hour_ns);
    let before_end = utc_day < dst_end_day
        || (utc_day == dst_end_day && within_day < 6 * hour_ns);
    let offset_hours = if after_start && before_end { -4 } else { -5 };

    let local_ns = utc_ns + offset_hours * hour_ns;
    let local_day = local_ns.div_euclid(day_ns);
    let local_minute = local_ns.rem_euclid(day_ns)
        / TimeDelta::from_mins(1).as_nanos();
    (local_day, local_minute as i32)
}

fn weekday_sunday_zero(days_since_epoch: i64) -> i64 {
    (days_since_epoch + 4).rem_euclid(7)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let adjusted_year = if month <= 2 { year - 1 } else { year };
    let era = (if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    })
    .div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let shifted = days_since_epoch + 719_468;
    let era = (if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    })
    .div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524
        - day_of_era / 146_096)
        / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    if month <= 2 {
        year += 1;
    }
    (year, month, day)
}

export_strategy!(UsCashFlowContinuation::new());
