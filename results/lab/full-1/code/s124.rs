use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, TimeInForce,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const LOOKBACK_DAYS: usize = 30;
const SESSION_MINUTES: usize = 360;
const ENTRY_EXPIRY_MINUTES: i64 = 60;
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum OrderKind {
    Entry,
    Exit,
}

struct TrackedOrder {
    id: tape_guest::OrderId,
    kind: OrderKind,
    remaining_raw: i64,
    submitted_ns: i64,
    cancel_requested: bool,
}

struct AsianMorningPressureFade {
    session_day: Option<i64>,
    session_started: bool,
    session_valid: bool,
    session_evaluated: bool,
    session_bars: usize,
    session_last_minute: i64,
    session_open_raw: i64,
    session_close_raw: i64,
    session_volume_raw: i128,
    history: VecDeque<(i64, i128)>,
    entry_day: Option<i64>,
    order: Option<TrackedOrder>,
    filled_position_raw: i128,
    completed_round_trips: u64,
}

impl AsianMorningPressureFade {
    fn reset_session(&mut self, day: i64) {
        self.session_day = Some(day);
        self.session_started = false;
        self.session_valid = false;
        self.session_evaluated = false;
        self.session_bars = 0;
        self.session_last_minute = -1;
        self.session_open_raw = 0;
        self.session_close_raw = 0;
        self.session_volume_raw = 0;
    }

    fn prior_volume_below_median(&self, day: i64, volume_raw: i128) -> bool {
        if self.history.len() != LOOKBACK_DAYS {
            return false;
        }

        let first_day = day.saturating_sub(LOOKBACK_DAYS as i64);
        for (index, (sample_day, _)) in self.history.iter().enumerate() {
            if *sample_day != first_day.saturating_add(index as i64) {
                return false;
            }
        }

        let mut values: Vec<i128> = self.history.iter().map(|(_, volume)| *volume).collect();
        values.sort_unstable();
        let middle = LOOKBACK_DAYS / 2;
        volume_raw.saturating_mul(2)
            < values[middle - 1].saturating_add(values[middle])
    }

    fn session_is_complete(&self) -> bool {
        self.session_started
            && self.session_valid
            && self.session_bars == SESSION_MINUTES
            && self.session_last_minute == 359
    }
}

impl Strategy for AsianMorningPressureFade {
    fn name(&self) -> String {
        "Asian-Morning Pressure Fade [wasm]".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        if minute_ns <= 0 || day_ns <= 0 {
            return;
        }
        let timestamp_ns = bar.close_ts.as_nanos();
        let day = timestamp_ns.div_euclid(day_ns);
        let minute = timestamp_ns.rem_euclid(day_ns).div_euclid(minute_ns);

        // A finished or cancelled order is no longer ours to track.
        if ctx.open_orders() == 0 {
            self.order = None;
        }

        if let Some(order) = self.order.as_mut() {
            let age_ns = timestamp_ns.saturating_sub(order.submitted_ns);
            let expiry_ns = minute_ns.saturating_mul(ENTRY_EXPIRY_MINUTES);
            if order.kind == OrderKind::Entry
                && !order.cancel_requested
                && age_ns >= expiry_ns
            {
                ctx.cancel(order.id);
                order.cancel_requested = true;
            }
        }

        if self.session_day != Some(day) {
            self.reset_session(day);
        }

        if minute == 0 {
            self.session_started = true;
            self.session_valid = true;
            self.session_evaluated = false;
            self.session_bars = 1;
            self.session_last_minute = 0;
            self.session_open_raw = bar.open.raw();
            self.session_close_raw = bar.close.raw();
            self.session_volume_raw = i128::from(bar.volume.raw());
        } else if (1..SESSION_MINUTES as i64).contains(&minute) && self.session_started {
            if minute != self.session_last_minute + 1 {
                self.session_valid = false;
            }
            self.session_bars = self.session_bars.saturating_add(1);
            self.session_last_minute = minute;
            self.session_close_raw = bar.close.raw();
            self.session_volume_raw = self
                .session_volume_raw
                .saturating_add(i128::from(bar.volume.raw()));
        }

        if minute == 359 && !self.session_evaluated {
            self.session_evaluated = true;

            let oldest_day = day.saturating_sub(LOOKBACK_DAYS as i64);
            while self
                .history
                .front()
                .is_some_and(|(sample_day, _)| *sample_day < oldest_day)
            {
                self.history.pop_front();
            }

            if self.session_is_complete() {
                let volume_raw = self.session_volume_raw;
                let open_raw = self.session_open_raw;
                let close_raw = self.session_close_raw;
                let signal = open_raw > 0
                    && i128::from(close_raw).saturating_mul(1_000)
                        <= i128::from(open_raw).saturating_mul(994)
                    && self.prior_volume_below_median(day, volume_raw);

                if signal
                    && self.entry_day != Some(day)
                    && ctx.position(INSTRUMENT).is_zero()
                    && self.order.is_none()
                    && ctx.open_orders() == 0
                {
                    let equity_budget = ctx.equity().mul_ratio(1, 5);
                    let cash = ctx.cash();
                    let budget = if cash < equity_budget {
                        cash
                    } else {
                        equity_budget
                    };
                    if budget.is_positive() {
                        let qty = ctx.affordable_qty(
                            INSTRUMENT,
                            bar.close,
                            budget,
                            SIZING_MARGIN_PPM,
                        );
                        if qty.is_positive() {
                            let id = ctx.submit(
                                OrderRequest::market(INSTRUMENT, Side::Buy, qty)
                                    .with_time_in_force(TimeInForce::Gtc),
                            );
                            self.order = Some(TrackedOrder {
                                id,
                                kind: OrderKind::Entry,
                                remaining_raw: qty.raw(),
                                submitted_ns: timestamp_ns,
                                cancel_requested: false,
                            });
                            self.entry_day = Some(day);
                        }
                    }
                }

                self.history.push_back((day, volume_raw));
            }
        }

        // The 09:59 close callback schedules the exit for the 10:00 bar.
        if minute == 599
            && ctx.position(INSTRUMENT).is_positive()
            && self.order.is_none()
            && ctx.open_orders() == 0
        {
            let qty = ctx.position(INSTRUMENT);
            if qty.is_positive() {
                let id = ctx.submit(
                    OrderRequest::market(INSTRUMENT, Side::Sell, qty)
                        .with_time_in_force(TimeInForce::Gtc),
                );
                self.order = Some(TrackedOrder {
                    id,
                    kind: OrderKind::Exit,
                    remaining_raw: qty.raw(),
                    submitted_ns: timestamp_ns,
                    cancel_requested: false,
                });
            }
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if let Some(order) = self.order.as_mut() {
            if order.id == fill.order_id {
                order.remaining_raw = order.remaining_raw.saturating_sub(fill.qty.raw()).max(0);
                if order.remaining_raw == 0 {
                    self.order = None;
                }
            }
        }

        match fill.side {
            Side::Buy => {
                self.filled_position_raw = self
                    .filled_position_raw
                    .saturating_add(i128::from(fill.qty.raw()));
            }
            Side::Sell => {
                let before = self.filled_position_raw;
                self.filled_position_raw = self
                    .filled_position_raw
                    .saturating_sub(i128::from(fill.qty.raw()));
                if before > 0 && self.filled_position_raw <= 0 {
                    self.filled_position_raw = 0;
                    self.completed_round_trips = self.completed_round_trips.saturating_add(1);
                }
            }
        }
    }

    fn on_finish(&mut self, ctx: &mut Ctx) {
        ctx.log(&format!(
            "Asian-Morning Pressure Fade completed round trips: {}",
            self.completed_round_trips
        ));
    }
}

export_strategy!(AsianMorningPressureFade {
    session_day: None,
    session_started: false,
    session_valid: false,
    session_evaluated: false,
    session_bars: 0,
    session_last_minute: -1,
    session_open_raw: 0,
    session_close_raw: 0,
    session_volume_raw: 0,
    history: VecDeque::with_capacity(LOOKBACK_DAYS + 1),
    entry_day: None,
    order: None,
    filled_position_raw: 0,
    completed_round_trips: 0,
});
