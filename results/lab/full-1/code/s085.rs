use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta, TimeInForce,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy)]
struct HourBar {
    high: Price,
    low: Price,
    close: Price,
}

struct HourAgg {
    bucket: i64,
    high: Price,
    low: Price,
    close: Price,
    volume: Qty,
}

impl HourAgg {
    fn new(bucket: i64, bar: &Bar) -> Self {
        Self {
            bucket,
            high: bar.high,
            low: bar.low,
            close: bar.close,
            volume: bar.volume,
        }
    }

    fn add(&mut self, bar: &Bar) {
        if bar.high > self.high {
            self.high = bar.high;
        }
        if bar.low < self.low {
            self.low = bar.low;
        }
        self.close = bar.close;
        self.volume = self.volume + bar.volume;
    }

    fn finish(self) -> HourBar {
        HourBar {
            high: self.high,
            low: self.low,
            close: self.close,
        }
    }
}

struct ConfirmedSwingStructure {
    active_hour: Option<HourAgg>,
    recent_hours: VecDeque<HourBar>,
    latest_swing_high: Option<(Price, u64)>,
    confirmed_lows: VecDeque<Price>,
    next_swing_high_id: u64,
    last_entered_high_id: Option<u64>,
    previous_hour_close: Option<Price>,
    entry_pending: bool,
    exit_pending: bool,
    last_attempt_ts: Option<i64>,
}

impl ConfirmedSwingStructure {
    fn new() -> Self {
        Self {
            active_hour: None,
            recent_hours: VecDeque::with_capacity(7),
            latest_swing_high: None,
            confirmed_lows: VecDeque::with_capacity(2),
            next_swing_high_id: 0,
            last_entered_high_id: None,
            previous_hour_close: None,
            entry_pending: false,
            exit_pending: false,
            last_attempt_ts: None,
        }
    }

    fn finish_active_hour(&mut self, ctx: &mut Ctx) {
        let Some(agg) = self.active_hour.take() else {
            return;
        };
        self.on_hour_close(ctx, agg.finish());
    }

    fn on_hour_close(&mut self, ctx: &mut Ctx, hour: HourBar) {
        // Save this before pushing `hour` into the deque, which takes ownership.
        let close = hour.close;
        self.recent_hours.push_back(hour);
        if self.recent_hours.len() > 7 {
            self.recent_hours.pop_front();
        }

        let mut new_swing_low = None;
        if self.recent_hours.len() == 7 {
            // With three bars on either side, the middle bar is now confirmed.
            let center = self.recent_hours[3];
            let is_high = (0..7).all(|i| i == 3 || self.recent_hours[i].high < center.high);
            let is_low = (0..7).all(|i| i == 3 || self.recent_hours[i].low > center.low);

            if is_high {
                self.next_swing_high_id = self.next_swing_high_id.wrapping_add(1);
                self.latest_swing_high = Some((center.high, self.next_swing_high_id));
            }
            if is_low {
                self.confirmed_lows.push_back(center.low);
                if self.confirmed_lows.len() > 2 {
                    self.confirmed_lows.pop_front();
                }
                new_swing_low = Some(center.low);
            }
        }

        if let Some(previous_close) = self.previous_hour_close {
            if let Some((swing_high, swing_id)) = self.latest_swing_high {
                let higher_lows = self.confirmed_lows.len() == 2
                    && self.confirmed_lows[0] < self.confirmed_lows[1];
                let flat = ctx.position(INSTRUMENT).is_zero();
                let crossed_up = previous_close <= swing_high && close > swing_high;
                if flat
                    && !self.exit_pending
                    && higher_lows
                    && crossed_up
                    && self.last_entered_high_id != Some(swing_id)
                {
                    self.last_entered_high_id = Some(swing_id);
                    self.entry_pending = true;
                }
            }

            if ctx.position(INSTRUMENT).is_positive() {
                let swing_low = new_swing_low.or_else(|| self.confirmed_lows.back().copied());
                if let Some(swing_low) = swing_low {
                    if previous_close >= swing_low && close < swing_low {
                        self.exit_pending = true;
                        self.entry_pending = false;
                    }
                }
            }
        }
        self.previous_hour_close = Some(close);
    }

    fn capped_qty(target: Qty, volume: Qty) -> Qty {
        let cap = volume.mul_ratio(1, 10);
        Qty::from_raw(target.raw().min(cap.raw()))
    }

    fn maybe_submit(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if self.exit_pending && ctx.position(INSTRUMENT).is_zero() {
            self.exit_pending = false;
        }
        if ctx.open_orders() != 0 {
            return;
        }

        let attempt_ts = bar.open_ts.as_nanos();
        if self.last_attempt_ts == Some(attempt_ts) {
            return;
        }

        if self.exit_pending {
            let position = ctx.position(INSTRUMENT);
            if !position.is_positive() {
                self.exit_pending = false;
                return;
            }
            let qty = Self::capped_qty(position, bar.volume);
            if qty.is_positive() {
                ctx.submit(
                    OrderRequest::market(INSTRUMENT, Side::Sell, qty)
                        .with_time_in_force(TimeInForce::Ioc),
                );
                self.last_attempt_ts = Some(attempt_ts);
            }
            return;
        }

        if self.entry_pending {
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if !affordable.is_positive() {
                self.entry_pending = false;
                return;
            }
            let qty = Self::capped_qty(affordable, bar.volume);
            if qty.is_positive() {
                ctx.submit(
                    OrderRequest::market(INSTRUMENT, Side::Buy, qty)
                        .with_time_in_force(TimeInForce::Ioc),
                );
                self.last_attempt_ts = Some(attempt_ts);
            }
        }
    }
}

impl Strategy for ConfirmedSwingStructure {
    fn name(&self) -> String {
        "confirmed-swing-structure".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let hour_ns = TimeDelta::from_hours(1).as_nanos();
        let bucket = bar.open_ts.as_nanos().div_euclid(hour_ns);

        if self
            .active_hour
            .as_ref()
            .is_some_and(|active| active.bucket != bucket)
        {
            self.finish_active_hour(ctx);
        }

        match self.active_hour.as_mut() {
            Some(active) => active.add(bar),
            None => self.active_hour = Some(HourAgg::new(bucket, bar)),
        }

        // A bar whose close reaches the next UTC hour boundary completes this hour.
        if bar.close_ts.as_nanos().div_euclid(hour_ns) > bucket {
            self.finish_active_hour(ctx);
        }

        self.maybe_submit(ctx, bar);
    }
}

export_strategy!(ConfirmedSwingStructure::new());
