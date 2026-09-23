use std::collections::BTreeMap;

use tape_guest::{
    Bar, Ctx, Fill, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const MINUTES_PER_DAY: i64 = 1_440;
const HISTORY_MINUTES: i64 = 29 * MINUTES_PER_DAY + 120;
const SIZING_MARGIN_PPM: i64 = 5_000;
const MAX_HOLDING_BARS: usize = 240;

#[derive(Clone, Copy)]
struct BarPoint {
    close_raw: i64,
    volume_raw: i64,
}

#[derive(Clone, Copy)]
enum PendingOrder {
    Entry,
    Exit,
}

struct FundingWindowFlushReversal {
    bars: BTreeMap<i64, BarPoint>,
    pending: Option<PendingOrder>,
    entry_price_raw: Option<i64>,
    entry_qty_raw: i128,
    entry_fill_ts: Option<i64>,
    holding_bars: usize,
    last_age_bar: Option<i64>,
    exit_triggered: bool,
}

impl FundingWindowFlushReversal {
    fn new() -> Self {
        Self {
            bars: BTreeMap::new(),
            pending: None,
            entry_price_raw: None,
            entry_qty_raw: 0,
            entry_fill_ts: None,
            holding_bars: 0,
            last_age_bar: None,
            exit_triggered: false,
        }
    }

    fn remember_bar(&mut self, minute: i64, bar: &Bar) {
        self.bars.insert(
            minute,
            BarPoint {
                close_raw: bar.close.raw(),
                volume_raw: bar.volume.raw(),
            },
        );

        let cutoff = minute - HISTORY_MINUTES;
        while self
            .bars
            .first_key_value()
            .is_some_and(|(oldest, _)| *oldest < cutoff)
        {
            self.bars.pop_first();
        }
    }

    fn window_volume(&self, event_minute: i64) -> Option<i128> {
        let mut total = 0_i128;
        for minute in (event_minute - 60)..event_minute {
            let point = self.bars.get(&minute)?;
            total += i128::from(point.volume_raw);
        }
        Some(total)
    }

    fn is_entry_signal(&self, event_minute: i64, confirmed_close_raw: i64) -> bool {
        let Some(start) = self.bars.get(&(event_minute - 60)) else {
            return false;
        };
        let Some(pre_event) = self.bars.get(&(event_minute - 1)) else {
            return false;
        };
        let Some(event_close) = self.bars.get(&event_minute) else {
            return false;
        };
        let Some(current_volume) = self.window_volume(event_minute) else {
            return false;
        };

        // Return from T-60 to T-1 must be at most -0.8%.
        if i128::from(pre_event.close_raw) * 1_000
            > i128::from(start.close_raw) * 992
        {
            return false;
        }

        // The T+15 close must be at least 0.25% above the T close.
        if i128::from(confirmed_close_raw) * 400
            < i128::from(event_close.close_raw) * 401
        {
            return false;
        }

        let mut prior_volumes = Vec::with_capacity(28);
        for days_ago in 1..=28_i64 {
            let prior_event = event_minute - days_ago * MINUTES_PER_DAY;
            let Some(total) = self.window_volume(prior_event) else {
                return false;
            };
            prior_volumes.push(total);
        }
        prior_volumes.sort_unstable();

        // For 28 observations, the median is the mean of the middle two.
        // Compare without rounding: current > 1.5 * median.
        4 * current_volume > 3 * (prior_volumes[13] + prior_volumes[14])
    }

    fn reset_trade(&mut self) {
        self.entry_price_raw = None;
        self.entry_qty_raw = 0;
        self.entry_fill_ts = None;
        self.holding_bars = 0;
        self.last_age_bar = None;
        self.exit_triggered = false;
    }

    fn volume_limited_qty(volume: Qty, wanted: Qty) -> Qty {
        if !volume.is_positive() || !wanted.is_positive() {
            return Qty::from_raw(0);
        }
        let cap = volume.mul_ratio(1, 10);
        if wanted.raw() < cap.raw() {
            wanted
        } else {
            cap
        }
    }
}

impl Strategy for FundingWindowFlushReversal {
    fn name(&self) -> String {
        "funding-window-flush-reversal".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let minute = bar.close_ts.as_nanos() / minute_ns;
        self.remember_bar(minute, bar);

        // Do not issue another order while an earlier order is still live.
        if ctx.open_orders() != 0 {
            return;
        }
        self.pending = None;

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            let (Some(entry_price_raw), Some(entry_fill_ts)) =
                (self.entry_price_raw, self.entry_fill_ts)
            else {
                return;
            };

            if bar.close_ts.as_nanos() > entry_fill_ts && self.last_age_bar != Some(minute) {
                self.holding_bars = self.holding_bars.saturating_add(1);
                self.last_age_bar = Some(minute);
            }

            let close_raw = i128::from(bar.close.raw());
            let entry_raw = i128::from(entry_price_raw);
            let reached_profit = close_raw * 200 >= entry_raw * 203;
            let reached_loss = close_raw * 200 <= entry_raw * 197;
            if reached_profit || reached_loss || self.holding_bars >= MAX_HOLDING_BARS {
                self.exit_triggered = true;
            }

            if self.exit_triggered && ctx.open_orders() == 0 {
                let qty = Self::volume_limited_qty(bar.volume, position);
                if qty.is_positive() {
                    let _ = ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
                    self.pending = Some(PendingOrder::Exit);
                }
            }
            return;
        }

        if !position.is_zero() {
            return;
        }
        self.reset_trade();

        let event_minute = minute - 15;
        let event_time_of_day = event_minute.rem_euclid(MINUTES_PER_DAY);
        if event_time_of_day != 0 && event_time_of_day != 480 && event_time_of_day != 960 {
            return;
        }
        if !self.is_entry_signal(event_minute, bar.close.raw()) {
            return;
        }
        if ctx.open_orders() != 0 {
            return;
        }

        // Reserve 2 bp for the specified slippage when sizing. The execution
        // engine's affordable-quantity calculation accounts for its configured fees.
        let sizing_price: Price = bar.close.mul_ratio(10_002, 10_000);
        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            sizing_price,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        let qty = Self::volume_limited_qty(bar.volume, affordable);
        if qty.is_positive() && ctx.open_orders() == 0 {
            let _ = ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            self.pending = Some(PendingOrder::Entry);
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Buy {
            let added_qty = i128::from(fill.qty.raw());
            if added_qty <= 0 {
                return;
            }

            let new_total = self.entry_qty_raw + added_qty;
            let weighted_price = match self.entry_price_raw {
                Some(old_price) if self.entry_qty_raw > 0 => {
                    (i128::from(old_price) * self.entry_qty_raw
                        + i128::from(fill.price.raw()) * added_qty)
                        / new_total
                }
                _ => i128::from(fill.price.raw()),
            };
            self.entry_price_raw = Some(weighted_price as i64);
            self.entry_qty_raw = new_total;

            let fill_ts = fill.ts.as_nanos();
            self.entry_fill_ts = Some(
                self.entry_fill_ts
                    .map_or(fill_ts, |existing| existing.min(fill_ts)),
            );
        } else if fill.side == Side::Sell && ctx.position(INSTRUMENT).is_zero() {
            self.reset_trade();
        }
    }
}

export_strategy!(FundingWindowFlushReversal::new());
