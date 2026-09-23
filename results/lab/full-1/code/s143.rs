use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, Fill, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, TimeInForce,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy)]
struct HourCandle {
    bucket: i64,
    close_raw: i64,
    high_raw: i64,
}

struct UpsideBurstCooldown {
    initialized: bool,
    active_bucket: Option<i64>,
    active_high_raw: Option<i64>,
    active_close_raw: Option<i64>,
    hourly_history: VecDeque<HourCandle>,
    previous_hour_high_raw: Option<i64>,

    entry_pending: bool,
    entry_target_raw: Option<i64>,
    exit_pending: bool,

    flat_since_ns: Option<i64>,
    flat_high_raw: Option<i64>,
    reentry_armed: bool,
}

impl UpsideBurstCooldown {
    fn new() -> Self {
        Self {
            initialized: false,
            active_bucket: None,
            active_high_raw: None,
            active_close_raw: None,
            hourly_history: VecDeque::with_capacity(8),
            previous_hour_high_raw: None,
            entry_pending: false,
            entry_target_raw: None,
            exit_pending: false,
            flat_since_ns: None,
            flat_high_raw: None,
            reentry_armed: false,
        }
    }

    fn schedule_entry(&mut self) {
        if !self.entry_pending {
            self.entry_pending = true;
            self.entry_target_raw = None;
        }
    }

    fn complete_exit(&mut self, ts_ns: i64) {
        self.exit_pending = false;
        self.entry_pending = false;
        self.entry_target_raw = None;
        self.flat_since_ns = Some(ts_ns);
        self.flat_high_raw = None;
        self.reentry_armed = false;
    }

    fn finish_active_hour(&mut self, ctx: &mut Ctx) {
        let Some(bucket) = self.active_bucket.take() else {
            return;
        };
        let (Some(high_raw), Some(close_raw)) =
            (self.active_high_raw.take(), self.active_close_raw.take())
        else {
            return;
        };

        let hour_ns = TimeDelta::from_mins(60).as_nanos();
        let end_ns = bucket.saturating_add(1).saturating_mul(hour_ns);
        self.on_hour_close(
            ctx,
            HourCandle {
                bucket,
                close_raw,
                high_raw,
            },
            end_ns,
        );
    }

    fn add_minute(&mut self, ctx: &mut Ctx, bar: &Bar) {
        let hour_ns = TimeDelta::from_mins(60).as_nanos();
        if hour_ns <= 0 {
            return;
        }

        // A bar whose close timestamp is exactly on the hour belongs to the
        // hour that just ended.
        let close_ns = bar.close_ts.as_nanos();
        let bucket = close_ns.saturating_sub(1).div_euclid(hour_ns);

        if self.active_bucket.is_some_and(|active| active != bucket) {
            self.finish_active_hour(ctx);
        }
        if self.active_bucket.is_none() {
            self.active_bucket = Some(bucket);
            self.active_high_raw = Some(bar.high.raw());
        } else if let Some(high) = self.active_high_raw.as_mut() {
            *high = (*high).max(bar.high.raw());
        }
        self.active_close_raw = Some(bar.close.raw());

        if close_ns.rem_euclid(hour_ns) == 0 {
            self.finish_active_hour(ctx);
        }
    }

    fn on_hour_close(&mut self, ctx: &mut Ctx, candle: HourCandle, end_ns: i64) {
        let prior_high = self.previous_hour_high_raw;
        let six_hours_ago_bucket = candle.bucket.saturating_sub(6);
        let close_six_hours_ago = self
            .hourly_history
            .iter()
            .find(|past| past.bucket == six_hours_ago_bucket)
            .map(|past| past.close_raw);

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() && !self.exit_pending {
            if let Some(old_close) = close_six_hours_ago {
                let rally = i128::from(candle.close_raw) * 100
                    >= i128::from(old_close) * 104;
                if rally {
                    self.exit_pending = true;
                    self.entry_pending = false;
                    self.entry_target_raw = None;
                }
            }
        } else if position.is_zero() && self.flat_since_ns.is_some() {
            let exit_ns = self.flat_since_ns.unwrap_or(end_ns);
            if end_ns > exit_ns {
                let tracked_high = self
                    .flat_high_raw
                    .map_or(candle.close_raw, |high| high.max(candle.close_raw));
                self.flat_high_raw = Some(tracked_high);

                let breakout = self.reentry_armed
                    && prior_high.is_some_and(|high| candle.close_raw > high);
                if breakout {
                    self.schedule_entry();
                } else if i128::from(candle.close_raw) * 1_000
                    <= i128::from(tracked_high) * 985
                {
                    self.reentry_armed = true;
                }
            }
        }

        self.hourly_history.push_back(candle);
        while self.hourly_history.len() > 8 {
            self.hourly_history.pop_front();
        }
        self.previous_hour_high_raw = Some(candle.high_raw);
    }

    fn submit_pending(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if ctx.open_orders() != 0 {
            return;
        }

        if self.exit_pending {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                ctx.submit(
                    OrderRequest::market(INSTRUMENT, Side::Sell, position)
                        .with_time_in_force(TimeInForce::Ioc),
                );
            } else {
                self.complete_exit(bar.close_ts.as_nanos());
            }
            return;
        }

        if !self.entry_pending {
            return;
        }

        if self.entry_target_raw.is_none() {
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if affordable.is_positive() {
                self.entry_target_raw = Some(affordable.raw());
            } else {
                return;
            }
        }

        let target_raw = self.entry_target_raw.unwrap_or(0);
        let held_raw = ctx.position(INSTRUMENT).raw().max(0);
        let remaining_raw = target_raw.saturating_sub(held_raw);
        if remaining_raw <= 0 {
            self.entry_pending = false;
            self.entry_target_raw = None;
            return;
        }

        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        let qty_raw = remaining_raw.min(affordable.raw().max(0));
        if qty_raw > 0 {
            ctx.submit(
                OrderRequest::market(
                    INSTRUMENT,
                    Side::Buy,
                    tape_guest::Qty::from_raw(qty_raw),
                )
                .with_time_in_force(TimeInForce::Ioc),
            );
        }
    }
}

impl Strategy for UpsideBurstCooldown {
    fn name(&self) -> String {
        "upside-burst-cooldown".to_string()
    }

    fn on_start(&mut self, ctx: &mut Ctx) {
        if ctx.position(INSTRUMENT).is_positive() {
            self.initialized = true;
        }
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        if !self.initialized {
            self.initialized = true;
            if ctx.position(INSTRUMENT).is_zero() && self.flat_since_ns.is_none() {
                self.schedule_entry();
            }
        }

        if self.exit_pending && ctx.position(INSTRUMENT).is_zero() {
            self.complete_exit(bar.close_ts.as_nanos());
        }

        self.add_minute(ctx, bar);

        if let Some(exit_ns) = self.flat_since_ns {
            let deadline_ns = exit_ns.saturating_add(TimeDelta::from_hours(24).as_nanos());
            if bar.close_ts.as_nanos() >= deadline_ns && ctx.position(INSTRUMENT).is_zero() {
                self.schedule_entry();
            }
        }

        self.submit_pending(ctx, bar);
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Buy {
            self.flat_since_ns = None;
            self.flat_high_raw = None;
            self.reentry_armed = false;

            if let Some(target_raw) = self.entry_target_raw {
                if ctx.position(INSTRUMENT).raw() >= target_raw {
                    self.entry_pending = false;
                    self.entry_target_raw = None;
                }
            }
        } else if fill.side == Side::Sell
            && self.exit_pending
            && ctx.position(INSTRUMENT).is_zero()
        {
            self.complete_exit(fill.ts.as_nanos());
        }
    }
}

export_strategy!(UpsideBurstCooldown::new());
