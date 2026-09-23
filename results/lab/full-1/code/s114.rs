use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta, Timestamp,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const FIVE_MIN_BARS_PER_THREE_HOURS: usize = 36;
const BASELINE_WINDOWS: usize = 240;
const HISTORY_CAPACITY: usize = 8_700;

#[derive(Clone, Copy)]
struct FiveBar {
    close_ts: Timestamp,
    high_raw: i64,
    low_raw: i64,
    close_raw: i64,
    volume_raw: i64,
}

struct MinuteAgg {
    count: usize,
    close_ts: Timestamp,
    high_raw: i64,
    low_raw: i64,
    close_raw: i64,
    volume_raw: i64,
}

impl MinuteAgg {
    fn from_bar(bar: &Bar) -> Self {
        Self {
            count: 1,
            close_ts: bar.close_ts,
            high_raw: bar.high.raw(),
            low_raw: bar.low.raw(),
            close_raw: bar.close.raw(),
            volume_raw: bar.volume.raw(),
        }
    }

    fn add(&mut self, bar: &Bar) {
        self.count += 1;
        self.close_ts = bar.close_ts;
        self.high_raw = self.high_raw.max(bar.high.raw());
        self.low_raw = self.low_raw.min(bar.low.raw());
        self.close_raw = bar.close.raw();
        self.volume_raw = self.volume_raw.saturating_add(bar.volume.raw());
    }

    fn completed(&self) -> FiveBar {
        FiveBar {
            close_ts: self.close_ts,
            high_raw: self.high_raw,
            low_raw: self.low_raw,
            close_raw: self.close_raw,
            volume_raw: self.volume_raw,
        }
    }
}

#[derive(Clone, Copy)]
struct SelloffEvent {
    start_ts: Timestamp,
    low_ts: Timestamp,
    low_raw: i64,
    peak_rv: f64,
    entered: bool,
}

#[derive(Clone, Copy)]
struct SelloffSignal {
    low_ts: Timestamp,
    low_raw: i64,
    realized_vol: f64,
}

struct StabilizedSelloffRecovery {
    minute_agg: Option<MinuteAgg>,
    history: VecDeque<FiveBar>,
    event: Option<SelloffEvent>,
    flag_was_active: bool,
    tracked_qty_raw: i128,
    entry_price: Option<Price>,
    entry_ts: Option<Timestamp>,
    trade_low_raw: Option<i64>,
    exit_requested: bool,
}

impl StabilizedSelloffRecovery {
    fn new() -> Self {
        Self {
            minute_agg: None,
            history: VecDeque::with_capacity(HISTORY_CAPACITY),
            event: None,
            flag_was_active: false,
            tracked_qty_raw: 0,
            entry_price: None,
            entry_ts: None,
            trade_low_raw: None,
            exit_requested: false,
        }
    }

    fn realized_volatility(&self) -> Option<f64> {
        if self.history.len() < 7 {
            return None;
        }
        let start = self.history.len() - 7;
        let mut sum_sq = 0.0;
        for i in start..(self.history.len() - 1) {
            let older = self.history[i].close_raw as f64;
            let newer = self.history[i + 1].close_raw as f64;
            if older <= 0.0 || newer <= 0.0 {
                return None;
            }
            let change = (newer / older).ln();
            sum_sq += change * change;
        }
        Some(sum_sq.sqrt())
    }

    fn selloff_signal(&self) -> Option<SelloffSignal> {
        let len = self.history.len();
        // 30 days of 3-hour observations, plus the trailing 3-hour window.
        if len < 8_676 {
            return None;
        }

        let latest = self.history[len - 1];
        let three_hours_ago = self.history[len - 1 - FIVE_MIN_BARS_PER_THREE_HOURS];
        if i128::from(latest.close_raw) * 100
            > i128::from(three_hours_ago.close_raw) * 98
        {
            return None;
        }

        let mut trailing_volume: i128 = 0;
        let mut low_raw = i64::MAX;
        let mut low_ts = latest.close_ts;
        for offset in 0..FIVE_MIN_BARS_PER_THREE_HOURS {
            let item = self.history[len - 1 - offset];
            trailing_volume += i128::from(item.volume_raw);
            if item.low_raw < low_raw {
                low_raw = item.low_raw;
                low_ts = item.close_ts;
            }
        }

        // Use the 240 non-overlapping 3-hour volume windows immediately
        // preceding the current trailing 3-hour window.
        let mut historical_volumes = Vec::with_capacity(BASELINE_WINDOWS);
        for window in 1..=BASELINE_WINDOWS {
            let ending_offset = window * FIVE_MIN_BARS_PER_THREE_HOURS;
            let mut volume: i128 = 0;
            for within_window in 0..FIVE_MIN_BARS_PER_THREE_HOURS {
                let offset = ending_offset + within_window;
                volume += i128::from(self.history[len - 1 - offset].volume_raw);
            }
            historical_volumes.push(volume);
        }
        historical_volumes.sort_unstable();
        let middle = BASELINE_WINDOWS / 2;
        let median_twice = historical_volumes[middle - 1] + historical_volumes[middle];
        if trailing_volume * 4 < median_twice * 3 {
            return None;
        }

        Some(SelloffSignal {
            low_ts,
            low_raw,
            realized_vol: self.realized_volatility()?,
        })
    }

    fn breakout_above_prior_two_hours(&self) -> bool {
        if self.history.len() < 25 {
            return false;
        }
        let latest_index = self.history.len() - 1;
        let mut highest = i64::MIN;
        for offset in 1..=24 {
            highest = highest.max(self.history[latest_index - offset].close_raw);
        }
        self.history[latest_index].close_raw > highest
    }

    fn elapsed_at_least(now: Timestamp, then: Timestamp, duration: TimeDelta) -> bool {
        now.as_nanos().saturating_sub(then.as_nanos()) >= duration.as_nanos()
    }

    fn clear_trade(&mut self) {
        self.tracked_qty_raw = 0;
        self.entry_price = None;
        self.entry_ts = None;
        self.trade_low_raw = None;
        self.exit_requested = false;
    }

    fn submit_exit_if_possible(&mut self, ctx: &mut Ctx) {
        let position = ctx.position(INSTRUMENT);
        if !position.is_positive() {
            return;
        }
        if ctx.open_orders() == 0 {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
        }
    }

    fn on_five_minute_bar(&mut self, ctx: &mut Ctx, bar: FiveBar, sizing_price: Price) {
        self.history.push_back(bar);
        while self.history.len() > HISTORY_CAPACITY {
            self.history.pop_front();
        }

        let signal = self.selloff_signal();
        let is_selloff = signal.is_some();
        let has_position = ctx.position(INSTRUMENT).is_positive() || self.tracked_qty_raw > 0;

        if is_selloff && !self.flag_was_active && !has_position {
            if let Some(signal) = signal {
                self.event = Some(SelloffEvent {
                    start_ts: bar.close_ts,
                    low_ts: signal.low_ts,
                    low_raw: signal.low_raw,
                    peak_rv: signal.realized_vol,
                    entered: false,
                });
            }
        }
        self.flag_was_active = is_selloff;

        if has_position {
            let hit_low = self
                .trade_low_raw
                .is_some_and(|low| bar.close_raw < low);
            let hit_target = self.entry_price.is_some_and(|entry| {
                i128::from(bar.close_raw) * 100 >= i128::from(entry.raw()) * 103
            });
            let hit_time = self
                .entry_ts
                .is_some_and(|entry_ts| Self::elapsed_at_least(bar.close_ts, entry_ts, TimeDelta::from_hours(48)));

            if hit_low || hit_target || hit_time {
                self.exit_requested = true;
                self.submit_exit_if_possible(ctx);
            }
            return;
        }

        let latest_rv = self.realized_volatility();
        let mut expired_unentered = false;
        if let Some(event) = self.event.as_mut() {
            let within_window = !Self::elapsed_at_least(
                bar.close_ts,
                event.start_ts,
                TimeDelta::from_hours(12),
            );
            if !within_window {
                if !event.entered {
                    expired_unentered = true;
                }
            } else {
                if bar.low_raw < event.low_raw {
                    event.low_raw = bar.low_raw;
                    event.low_ts = bar.close_ts;
                }
                if let Some(rv) = latest_rv {
                    event.peak_rv = event.peak_rv.max(rv);
                }
            }
        }
        if expired_unentered {
            self.event = None;
        }

        let Some(event) = self.event else {
            return;
        };
        if event.entered
            || !Self::elapsed_at_least(bar.close_ts, event.low_ts, TimeDelta::from_hours(2))
            || ctx.open_orders() != 0
            || self.tracked_qty_raw > 0
            || !ctx.position(INSTRUMENT).is_zero()
            || !self.breakout_above_prior_two_hours()
        {
            return;
        }
        let Some(rv) = latest_rv else {
            return;
        };
        if rv > event.peak_rv * 0.70 {
            return;
        }

        let qty = ctx.affordable_qty(
            INSTRUMENT,
            sizing_price,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        if !qty.is_positive() || ctx.open_orders() != 0 {
            return;
        }

        if let Some(active_event) = self.event.as_mut() {
            active_event.entered = true;
        }
        self.trade_low_raw = Some(event.low_raw);
        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
    }
}

impl Strategy for StabilizedSelloffRecovery {
    fn name(&self) -> String {
        "stabilized-selloff-recovery".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        if self.exit_requested {
            if ctx.position(INSTRUMENT).is_zero() {
                self.clear_trade();
            } else {
                self.submit_exit_if_possible(ctx);
            }
        }

        if self.minute_agg.is_none() {
            self.minute_agg = Some(MinuteAgg::from_bar(bar));
            return;
        }

        let agg = self.minute_agg.as_mut().expect("aggregation exists");
        agg.add(bar);
        if agg.count == 5 {
            let completed = agg.completed();
            self.minute_agg = None;
            self.on_five_minute_bar(ctx, completed, bar.close);
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        let fill_qty = i128::from(fill.qty.raw());
        if fill.side == Side::Buy {
            let old_qty = self.tracked_qty_raw;
            let new_qty = old_qty + fill_qty;
            if old_qty <= 0 {
                self.entry_price = Some(fill.price);
                self.entry_ts = Some(fill.ts);
            } else if new_qty > 0 {
                let old_value = i128::from(self.entry_price.unwrap_or(fill.price).raw()) * old_qty;
                let added_value = i128::from(fill.price.raw()) * fill_qty;
                self.entry_price = Some(Price::from_raw(((old_value + added_value) / new_qty) as i64));
            }
            self.tracked_qty_raw = new_qty;
        } else {
            self.tracked_qty_raw -= fill_qty;
            if self.tracked_qty_raw <= 0 {
                self.clear_trade();
            }
        }
    }
}

export_strategy!(StabilizedSelloffRecovery::new());
