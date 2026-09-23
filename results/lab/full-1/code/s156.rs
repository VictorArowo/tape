use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, Money, OrderRequest, Price, Qty, Side, Strategy, TimeDelta,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SCALE: i128 = 100_000_000;
const SIZING_MARGIN_PPM: i64 = 5_000;
const VOLUME_LOOKBACK: usize = 96;
const ATR_LOOKBACK: usize = 20;

struct Candle15 {
    open: Price,
    high: Price,
    low: Price,
    close: Price,
    volume: Qty,
    close_ts_ns: i64,
}

struct Aggregate {
    bucket: i64,
    first_minute: i64,
    last_minute: i64,
    count: u8,
    consecutive: bool,
    open: Price,
    high: Price,
    low: Price,
    close: Price,
    volume_raw: i64,
    close_ts_ns: i64,
}

impl Aggregate {
    fn new(bucket: i64, minute: i64, bar: &Bar) -> Self {
        Self {
            bucket,
            first_minute: minute,
            last_minute: minute,
            count: 1,
            consecutive: true,
            open: bar.open,
            high: bar.high,
            low: bar.low,
            close: bar.close,
            volume_raw: bar.volume.raw(),
            close_ts_ns: bar.close_ts.as_nanos(),
        }
    }

    fn add(&mut self, minute: i64, bar: &Bar) {
        if minute != self.last_minute + 1 {
            self.consecutive = false;
        }
        self.last_minute = minute;
        self.count = self.count.saturating_add(1);
        if bar.high.raw() > self.high.raw() {
            self.high = bar.high;
        }
        if bar.low.raw() < self.low.raw() {
            self.low = bar.low;
        }
        self.close = bar.close;
        self.volume_raw = self.volume_raw.saturating_add(bar.volume.raw());
        self.close_ts_ns = bar.close_ts.as_nanos();
    }

    fn is_full(&self) -> bool {
        self.count == 15
            && self.consecutive
            && self.first_minute == self.bucket * 15
            && self.last_minute == self.bucket * 15 + 14
    }

    fn into_candle(self) -> Candle15 {
        Candle15 {
            open: self.open,
            high: self.high,
            low: self.low,
            close: self.close,
            volume: Qty::from_raw(self.volume_raw),
            close_ts_ns: self.close_ts_ns,
        }
    }
}

#[derive(Clone, Copy)]
struct Event {
    midpoint_twice: i128,
    low_raw: i64,
    atr_sum_raw: i128,
}

struct Position {
    stop: Price,
    target: Price,
    completed_bars: u16,
    exit_due: bool,
    entry_qty_raw: i128,
    entry_value_raw: i128,
}

struct BidAbsorptionReversal {
    aggregate: Option<Aggregate>,
    true_ranges: VecDeque<i64>,
    volumes: VecDeque<i64>,
    previous_close: Option<Price>,
    pending_event: Option<Event>,
    pending_entry_stop: Option<Price>,
    position: Option<Position>,
    exit_order_pending: bool,
    last_exit_ns: Option<i64>,
}

impl BidAbsorptionReversal {
    fn new() -> Self {
        Self {
            aggregate: None,
            true_ranges: VecDeque::with_capacity(ATR_LOOKBACK),
            volumes: VecDeque::with_capacity(VOLUME_LOOKBACK),
            previous_close: None,
            pending_event: None,
            pending_entry_stop: None,
            position: None,
            exit_order_pending: false,
            last_exit_ns: None,
        }
    }

    fn is_flat(&self, ctx: &Ctx) -> bool {
        self.position.is_none()
            && ctx.position(INSTRUMENT).is_zero()
            && self.pending_entry_stop.is_none()
            && ctx.open_orders() == 0
    }

    fn cooldown_complete(&self, at_ns: i64) -> bool {
        match self.last_exit_ns {
            None => true,
            Some(exit_ns) => {
                at_ns.saturating_sub(exit_ns) >= TimeDelta::from_hours(4).as_nanos()
            }
        }
    }

    fn detect_event(&self, candle: &Candle15) -> Option<Event> {
        if self.true_ranges.len() < ATR_LOOKBACK || self.volumes.len() < VOLUME_LOOKBACK {
            return None;
        }

        let atr_sum_raw: i128 = self.true_ranges.iter().map(|value| i128::from(*value)).sum();
        if atr_sum_raw <= 0 {
            return None;
        }

        let mut sorted_volumes: Vec<i64> = self.volumes.iter().copied().collect();
        sorted_volumes.sort_unstable();
        let median_pair_sum =
            i128::from(sorted_volumes[47]) + i128::from(sorted_volumes[48]);

        let range = i128::from(candle.high.raw()) - i128::from(candle.low.raw());
        if range <= 0 {
            return None;
        }
        let volume = i128::from(candle.volume.raw());
        let open = i128::from(candle.open.raw());
        let close = i128::from(candle.close.raw());
        let low = i128::from(candle.low.raw());
        let high = i128::from(candle.high.raw());
        let lower_body = open.min(close);
        let lower_wick = lower_body - low;

        // ATR is the mean of the preceding 20 ranges; volume median excludes this candle.
        if volume * 2 < median_pair_sum
            || range * 40 < atr_sum_raw * 3
            || lower_wick * 5 < range * 3
            || (close - low) * 10 < range * 7
        {
            return None;
        }

        Some(Event {
            midpoint_twice: high + low,
            low_raw: candle.low.raw(),
            atr_sum_raw,
        })
    }

    fn true_range_raw(&self, candle: &Candle15) -> i64 {
        let high = i128::from(candle.high.raw());
        let low = i128::from(candle.low.raw());
        let mut tr = high - low;
        if let Some(previous_close) = self.previous_close {
            let previous = i128::from(previous_close.raw());
            tr = tr.max((high - previous).abs()).max((low - previous).abs());
        }
        tr.clamp(0, i128::from(i64::MAX)) as i64
    }

    fn push_history(&mut self, candle: &Candle15, tr_raw: i64) {
        self.true_ranges.push_back(tr_raw);
        while self.true_ranges.len() > ATR_LOOKBACK {
            self.true_ranges.pop_front();
        }
        self.volumes.push_back(candle.volume.raw());
        while self.volumes.len() > VOLUME_LOOKBACK {
            self.volumes.pop_front();
        }
        self.previous_close = Some(candle.close);
    }

    fn submit_entry(&mut self, ctx: &mut Ctx, candle: &Candle15, event: Event) -> bool {
        if ctx.open_orders() != 0 || !ctx.position(INSTRUMENT).is_zero() {
            return false;
        }

        // ATR is sum/20, so 0.1 ATR is sum/200 in raw price units.
        let stop_raw = i128::from(event.low_raw) - event.atr_sum_raw / 200;
        let entry_raw = i128::from(candle.close.raw());
        let distance_raw = entry_raw - stop_raw;
        let equity = ctx.equity();
        if stop_raw <= 0 || distance_raw <= 0 || entry_raw <= 0 || equity.raw() <= 0 {
            return false;
        }

        let risk_money_raw = i128::from(equity.raw()) * 5 / 1_000;
        let risk_qty_raw = risk_money_raw * SCALE / distance_raw;
        let equity_cap_qty_raw = i128::from(equity.raw()) * SCALE / entry_raw;
        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            candle.close,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        let qty_raw = risk_qty_raw
            .min(equity_cap_qty_raw)
            .min(i128::from(affordable.raw()));
        if qty_raw <= 0 || qty_raw > i128::from(i64::MAX) {
            return false;
        }

        let stop = Price::from_raw(stop_raw as i64);
        let qty = Qty::from_raw(qty_raw as i64);
        self.pending_entry_stop = Some(stop);
        let _ = ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        true
    }

    fn submit_exit_if_due(&mut self, ctx: &mut Ctx) {
        let due = self
            .position
            .as_ref()
            .map(|position| position.exit_due)
            .unwrap_or(false);
        if !due || self.exit_order_pending || ctx.open_orders() != 0 {
            return;
        }
        let qty = ctx.position(INSTRUMENT);
        if qty.is_positive() {
            let _ = ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
            self.exit_order_pending = true;
        }
    }

    fn complete_candle(&mut self, ctx: &mut Ctx, candle: &Candle15) {
        let tr_raw = self.true_range_raw(candle);
        let current_event = self.detect_event(candle);

        if let Some(position) = self.position.as_mut() {
            position.completed_bars = position.completed_bars.saturating_add(1);
            if candle.close.raw() <= position.stop.raw()
                || candle.close.raw() >= position.target.raw()
                || position.completed_bars >= 16
            {
                position.exit_due = true;
            }
        }

        let prior_event = self.pending_event.take();
        let mut submitted_entry = false;
        if let Some(event) = prior_event {
            let confirmed = i128::from(candle.close.raw()) * 2 > event.midpoint_twice;
            if confirmed
                && self.is_flat(ctx)
                && self.cooldown_complete(candle.close_ts_ns)
            {
                submitted_entry = self.submit_entry(ctx, candle, event);
            }
        }

        if !submitted_entry
            && current_event.is_some()
            && self.is_flat(ctx)
            && self.cooldown_complete(candle.close_ts_ns)
        {
            self.pending_event = current_event;
        }

        self.push_history(candle, tr_raw);
        self.submit_exit_if_due(ctx);
    }

    fn add_minute(&mut self, ctx: &mut Ctx, bar: &Bar) {
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let open_minute = bar.open_ts.as_nanos().div_euclid(minute_ns);
        let bucket = open_minute.div_euclid(15);

        let new_bucket = match self.aggregate.as_ref() {
            None => true,
            Some(aggregate) => aggregate.bucket != bucket,
        };
        if new_bucket {
            self.aggregate = Some(Aggregate::new(bucket, open_minute, bar));
        } else if let Some(aggregate) = self.aggregate.as_mut() {
            aggregate.add(open_minute, bar);
        }

        let close_minute = bar.close_ts.as_nanos().div_euclid(minute_ns);
        if close_minute.rem_euclid(15) == 0 {
            let ending_bucket = (close_minute - 1).div_euclid(15);
            if let Some(aggregate) = self.aggregate.take() {
                if aggregate.bucket == ending_bucket && aggregate.is_full() {
                    self.complete_candle(ctx, &aggregate.into_candle());
                }
            }
        }
    }
}

impl Strategy for BidAbsorptionReversal {
    fn name(&self) -> String {
        "Bid-Absorption Reversal".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        if self.pending_entry_stop.is_some() && ctx.open_orders() == 0 {
            self.pending_entry_stop = None;
        }
        if self.exit_order_pending && ctx.open_orders() == 0 {
            self.exit_order_pending = false;
        }
        self.submit_exit_if_due(ctx);
        self.add_minute(ctx, bar);
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        match fill.side {
            Side::Buy => {
                let stop = match self
                    .pending_entry_stop
                    .or_else(|| self.position.as_ref().map(|position| position.stop))
                {
                    Some(stop) => stop,
                    None => return,
                };
                let qty_raw = i128::from(fill.qty.raw());
                if qty_raw <= 0 {
                    return;
                }
                let fill_price_raw = i128::from(fill.price.raw());
                if let Some(position) = self.position.as_mut() {
                    position.entry_qty_raw += qty_raw;
                    position.entry_value_raw += fill_price_raw * qty_raw;
                    let average_raw = position.entry_value_raw / position.entry_qty_raw;
                    let risk_per_unit = average_raw - i128::from(position.stop.raw());
                    position.target = Price::from_raw(
                        (average_raw + 2 * risk_per_unit)
                            .clamp(i128::from(i64::MIN), i128::from(i64::MAX))
                            as i64,
                    );
                    if fill.price.raw() <= position.stop.raw() {
                        position.exit_due = true;
                    }
                } else {
                    let risk_per_unit = fill_price_raw - i128::from(stop.raw());
                    let target_raw = fill_price_raw + 2 * risk_per_unit;
                    self.position = Some(Position {
                        stop,
                        target: Price::from_raw(
                            target_raw.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64,
                        ),
                        completed_bars: 0,
                        exit_due: fill.price.raw() <= stop.raw(),
                        entry_qty_raw: qty_raw,
                        entry_value_raw: fill_price_raw * qty_raw,
                    });
                }
                self.submit_exit_if_due(ctx);
            }
            Side::Sell => {
                if ctx.position(INSTRUMENT).is_zero() {
                    self.position = None;
                    self.exit_order_pending = false;
                    self.pending_event = None;
                    self.last_exit_ns = Some(fill.ts.as_nanos());
                } else {
                    self.exit_order_pending = false;
                    if let Some(position) = self.position.as_mut() {
                        position.exit_due = true;
                    }
                }
            }
        }
    }
}

export_strategy!(BidAbsorptionReversal::new());
