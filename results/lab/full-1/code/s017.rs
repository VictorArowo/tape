use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, TimeInForce,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const WINDOW_START_MINUTE: i64 = 14 * 60;
const SIGNAL_MINUTE: i64 = 16 * 60;
const EXIT_MINUTE: i64 = 20 * 60;
const WINDOW_BARS: usize = 120;
const LOOKBACK_DAYS: usize = 20;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct SessionOverlapContinuation {
    current_day: Option<i64>,
    window_open: Option<Price>,
    window_close: Option<Price>,
    window_volume: i128,
    window_count: usize,
    window_last_minute: Option<i64>,
    window_valid: bool,
    prior_volumes: VecDeque<i128>,
    evaluated_day: Option<i64>,
    entry_day: Option<i64>,
}

impl SessionOverlapContinuation {
    fn new() -> Self {
        Self {
            current_day: None,
            window_open: None,
            window_close: None,
            window_volume: 0,
            window_count: 0,
            window_last_minute: None,
            window_valid: false,
            prior_volumes: VecDeque::with_capacity(LOOKBACK_DAYS),
            evaluated_day: None,
            entry_day: None,
        }
    }

    fn reset_window(&mut self) {
        self.window_open = None;
        self.window_close = None;
        self.window_volume = 0;
        self.window_count = 0;
        self.window_last_minute = None;
        self.window_valid = false;
    }

    fn begin_day(&mut self, day: i64) {
        if let Some(previous_day) = self.current_day {
            let expected_last = previous_day * 1440 + (SIGNAL_MINUTE - 1);
            if self.window_valid
                && self.window_count == WINDOW_BARS
                && self.window_last_minute == Some(expected_last)
            {
                self.prior_volumes.push_back(self.window_volume);
                if self.prior_volumes.len() > LOOKBACK_DAYS {
                    self.prior_volumes.pop_front();
                }
            }
        }

        self.current_day = Some(day);
        self.reset_window();
    }

    fn record_window_bar(&mut self, bar: &Bar, absolute_minute: i64, minute_of_day: i64) {
        if minute_of_day == WINDOW_START_MINUTE {
            self.window_open = Some(bar.open);
            self.window_close = Some(bar.close);
            self.window_volume = i128::from(bar.volume.raw());
            self.window_count = 1;
            self.window_last_minute = Some(absolute_minute);
            self.window_valid = true;
        } else if (WINDOW_START_MINUTE + 1..SIGNAL_MINUTE).contains(&minute_of_day) {
            if self.window_valid
                && self.window_last_minute == Some(absolute_minute - 1)
            {
                self.window_close = Some(bar.close);
                self.window_volume += i128::from(bar.volume.raw());
                self.window_count += 1;
                self.window_last_minute = Some(absolute_minute);
            } else {
                self.window_valid = false;
            }
        }
    }

    fn signal_is_valid(&self, day: i64) -> bool {
        if self.prior_volumes.len() != LOOKBACK_DAYS
            || !self.window_valid
            || self.window_count != WINDOW_BARS
            || self.window_last_minute != Some(day * 1440 + SIGNAL_MINUTE - 1)
        {
            return false;
        }

        let (Some(open), Some(close)) = (self.window_open, self.window_close) else {
            return false;
        };
        if !open.is_positive() {
            return false;
        }

        // close / open - 1 must be strictly greater than 0.30%.
        let return_is_strong =
            i128::from(close.raw()) * 1_000 > i128::from(open.raw()) * 1_003;
        if !return_is_strong {
            return false;
        }

        let mut sorted: Vec<i128> = self.prior_volumes.iter().copied().collect();
        sorted.sort_unstable();
        // For 20 observations, compare against the exact average of the two middle values.
        self.window_volume * 2 > sorted[9] + sorted[10]
    }
}

impl Strategy for SessionOverlapContinuation {
    fn name(&self) -> String {
        "session-overlap-continuation".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let open_absolute_minute = bar.open_ts.as_nanos().div_euclid(minute_ns);
        let open_day = open_absolute_minute.div_euclid(1440);
        let open_minute_of_day = open_absolute_minute.rem_euclid(1440);

        if self.current_day != Some(open_day) {
            self.begin_day(open_day);
        }
        self.record_window_bar(bar, open_absolute_minute, open_minute_of_day);

        let close_absolute_minute = bar.close_ts.as_nanos().div_euclid(minute_ns);
        let close_minute_of_day = close_absolute_minute.rem_euclid(1440);

        // Submit the exit at the bar closing at 20:00, so the market order is
        // available for the next bar's 20:00 open. Retry on later bars if an
        // IOC order was only partially filled.
        if let Some(entry_day) = self.entry_day {
            let exit_deadline = entry_day
                .saturating_mul(1440)
                .saturating_add(EXIT_MINUTE);
            if close_absolute_minute >= exit_deadline {
                let position = ctx.position(INSTRUMENT);
                if position.is_zero() {
                    self.entry_day = None;
                } else if position.is_positive() && ctx.open_orders() == 0 {
                    ctx.submit(
                        OrderRequest::market(INSTRUMENT, Side::Sell, position)
                            .with_time_in_force(TimeInForce::Ioc),
                    );
                }
            }
        }

        if close_minute_of_day != SIGNAL_MINUTE
            || close_absolute_minute.div_euclid(1440) != open_day
            || self.evaluated_day == Some(open_day)
        {
            return;
        }
        self.evaluated_day = Some(open_day);

        if !self.signal_is_valid(open_day)
            || self.entry_day.is_some()
            || !ctx.position(INSTRUMENT).is_zero()
            || ctx.open_orders() != 0
            || !bar.close.is_positive()
        {
            return;
        }

        let qty = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        if qty.is_positive() {
            ctx.submit(
                OrderRequest::market(INSTRUMENT, Side::Buy, qty)
                    .with_time_in_force(TimeInForce::Ioc),
            );
            self.entry_day = Some(open_day);
        }
    }
}

export_strategy!(SessionOverlapContinuation::new());
