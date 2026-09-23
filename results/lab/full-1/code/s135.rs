use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeInForce,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const EMA_50_PERIOD: usize = 72_000;
const EMA_20_PERIOD: usize = 28_800;
const TREND_LOOKBACK: usize = 10_080;
const PULLBACK_LOOKBACK: usize = 10_080;
const ENTRY_LOOKBACK: usize = 360;
const SIZING_MARGIN_PPM: i64 = 5_000;
const SCALE: f64 = 100_000_000.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Buy,
    Sell,
}

struct Ema {
    alpha: f64,
    value: Option<f64>,
}

impl Ema {
    fn new(period: usize) -> Self {
        Self {
            alpha: 2.0 / (period as f64 + 1.0),
            value: None,
        }
    }

    fn push(&mut self, price: Price) -> f64 {
        let price = price.to_f64();
        let value = match self.value {
            Some(previous) => self.alpha * price + (1.0 - self.alpha) * previous,
            None => price,
        };
        self.value = Some(value);
        value
    }
}

struct TrendConditionedPullback {
    ema_50: Ema,
    ema_20: Ema,
    ema_50_history: VecDeque<f64>,
    long_highs: VecDeque<(usize, i64)>,
    short_highs: VecDeque<(usize, i64)>,
    bars_seen: usize,
    setup_armed: bool,
    setup_high_raw: Option<i64>,
    rearm_blocked: bool,
    pending: Option<Direction>,
    pending_remaining: Qty,
    entry_cost_raw_qty: i128,
    entry_qty_raw: i128,
}

impl TrendConditionedPullback {
    fn new() -> Self {
        Self {
            ema_50: Ema::new(EMA_50_PERIOD),
            ema_20: Ema::new(EMA_20_PERIOD),
            ema_50_history: VecDeque::with_capacity(TREND_LOOKBACK + 1),
            long_highs: VecDeque::with_capacity(PULLBACK_LOOKBACK + 1),
            short_highs: VecDeque::with_capacity(ENTRY_LOOKBACK + 1),
            bars_seen: 0,
            setup_armed: false,
            setup_high_raw: None,
            rearm_blocked: false,
            pending: None,
            pending_remaining: Qty::from_raw(0),
            entry_cost_raw_qty: 0,
            entry_qty_raw: 0,
        }
    }

    fn push_max(queue: &mut VecDeque<(usize, i64)>, index: usize, value: i64) {
        while let Some((_, back_value)) = queue.back() {
            if *back_value <= value {
                queue.pop_back();
            } else {
                break;
            }
        }
        queue.push_back((index, value));
    }

    fn submit_pending(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if ctx.open_orders() != 0 {
            return;
        }
        let Some(direction) = self.pending else {
            return;
        };

        let mut requested_raw = self.pending_remaining.raw();
        if requested_raw <= 0 {
            self.pending = None;
            self.pending_remaining = Qty::from_raw(0);
            return;
        }

        // Orders are issued after this bar closes. Limit each issued IOC slice
        // to 10% of the most recently completed bar's volume.
        let volume_cap_raw = bar.volume.raw() / 10;
        if volume_cap_raw <= 0 {
            return;
        }
        requested_raw = requested_raw.min(volume_cap_raw);

        let side = match direction {
            Direction::Buy => {
                let affordable = ctx.affordable_qty(
                    INSTRUMENT,
                    bar.close,
                    ctx.cash(),
                    SIZING_MARGIN_PPM,
                );
                requested_raw = requested_raw.min(affordable.raw());
                Side::Buy
            }
            Direction::Sell => {
                let position = ctx.position(INSTRUMENT);
                if !position.is_positive() {
                    self.pending = None;
                    self.pending_remaining = Qty::from_raw(0);
                    return;
                }
                requested_raw = requested_raw.min(position.raw());
                Side::Sell
            }
        };

        if requested_raw > 0 {
            ctx.submit(
                OrderRequest::market(INSTRUMENT, side, Qty::from_raw(requested_raw))
                    .with_time_in_force(TimeInForce::Ioc),
            );
        }
    }

    fn pullback_qualifies(close_raw: i64, high_raw: i64) -> bool {
        i128::from(close_raw) * 100 <= i128::from(high_raw) * 97
            && i128::from(close_raw) * 100 >= i128::from(high_raw) * 92
    }
}

impl Strategy for TrendConditionedPullback {
    fn name(&self) -> String {
        "trend-conditioned-pullback-re-entry [wasm]".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let ema_50 = self.ema_50.push(bar.close);
        let ema_20 = self.ema_20.push(bar.close);
        let ema_50_lag = if self.ema_50_history.len() >= TREND_LOOKBACK {
            self.ema_50_history.front().copied()
        } else {
            None
        };

        let long_cutoff = self.bars_seen.saturating_sub(PULLBACK_LOOKBACK);
        while self
            .long_highs
            .front()
            .is_some_and(|(index, _)| *index < long_cutoff)
        {
            self.long_highs.pop_front();
        }
        let short_cutoff = self.bars_seen.saturating_sub(ENTRY_LOOKBACK);
        while self
            .short_highs
            .front()
            .is_some_and(|(index, _)| *index < short_cutoff)
        {
            self.short_highs.pop_front();
        }

        let enough_long_history = self.bars_seen >= PULLBACK_LOOKBACK;
        let enough_short_history = self.bars_seen >= ENTRY_LOOKBACK;
        let rolling_high = if enough_long_history {
            self.long_highs.front().map(|(_, value)| *value)
        } else {
            None
        };
        let short_high = if enough_short_history {
            self.short_highs.front().map(|(_, value)| *value)
        } else {
            None
        };
        let uptrend = ema_50 > ema_20
            && ema_50_lag.is_some_and(|lagged| ema_50 > lagged);

        if ctx.open_orders() == 0 {
            let position = ctx.position(INSTRUMENT);

            if position.is_positive() {
                self.setup_armed = false;
                self.setup_high_raw = None;

                let ema_exit = bar.close.to_f64() < ema_20;
                let loss_exit = self.entry_qty_raw > 0
                    && i128::from(bar.close.raw()) * 100 * self.entry_qty_raw
                        <= self.entry_cost_raw_qty * 92;

                if ema_exit || loss_exit {
                    self.pending = Some(Direction::Sell);
                    self.pending_remaining = position;
                    self.rearm_blocked = true;
                }

                if self.pending == Some(Direction::Sell) {
                    self.submit_pending(ctx, bar);
                } else if self.pending == Some(Direction::Buy) {
                    self.submit_pending(ctx, bar);
                }
            } else if self.pending == Some(Direction::Sell) {
                // The sell has completed. Do not arm a new setup on this bar.
                self.pending = None;
                self.pending_remaining = Qty::from_raw(0);
                self.entry_cost_raw_qty = 0;
                self.entry_qty_raw = 0;
            } else if self.pending == Some(Direction::Buy) {
                self.submit_pending(ctx, bar);
            } else {
                let mut released_rearm_block = false;
                if self.rearm_blocked {
                    if let Some(high_raw) = rolling_high {
                        if i128::from(bar.close.raw()) * 100 >= i128::from(high_raw) * 97 {
                            self.rearm_blocked = false;
                            released_rearm_block = true;
                        }
                    }
                }

                if !released_rearm_block {
                    if self.setup_armed {
                        let fallen_too_far = self.setup_high_raw.is_some_and(|high_raw| {
                            i128::from(bar.close.raw()) * 100 < i128::from(high_raw) * 92
                        });
                        if !uptrend || fallen_too_far {
                            self.setup_armed = false;
                            self.setup_high_raw = None;
                        }
                    }

                    if !self.setup_armed && !self.rearm_blocked && uptrend {
                        if let Some(high_raw) = rolling_high {
                            if Self::pullback_qualifies(bar.close.raw(), high_raw) {
                                self.setup_armed = true;
                                self.setup_high_raw = Some(high_raw);
                            }
                        }
                    }

                    if self.setup_armed && uptrend {
                        if let Some(trigger_high) = short_high {
                            if bar.close.raw() > trigger_high && bar.close.is_positive() {
                                let equity = ctx.equity().to_f64();
                                let price = bar.close.to_f64();
                                if equity > 0.0 && price > 0.0 {
                                    let target_raw = (equity * 0.95 / price * SCALE) as i64;
                                    if target_raw > 0 {
                                        let target = Qty::from_raw(target_raw);
                                        let affordable = ctx.affordable_qty(
                                            INSTRUMENT,
                                            bar.close,
                                            ctx.cash(),
                                            SIZING_MARGIN_PPM,
                                        );
                                        let qty_raw = target.raw().min(affordable.raw());
                                        if qty_raw > 0 {
                                            self.entry_cost_raw_qty = 0;
                                            self.entry_qty_raw = 0;
                                            self.pending = Some(Direction::Buy);
                                            self.pending_remaining = Qty::from_raw(qty_raw);
                                            self.setup_armed = false;
                                            self.setup_high_raw = None;
                                            self.submit_pending(ctx, bar);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        self.ema_50_history.push_back(ema_50);
        if self.ema_50_history.len() > TREND_LOOKBACK {
            self.ema_50_history.pop_front();
        }
        Self::push_max(&mut self.long_highs, self.bars_seen, bar.high.raw());
        Self::push_max(&mut self.short_highs, self.bars_seen, bar.high.raw());
        self.bars_seen = self.bars_seen.saturating_add(1);
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        let matching_pending = matches!(
            (self.pending, fill.side),
            (Some(Direction::Buy), Side::Buy) | (Some(Direction::Sell), Side::Sell)
        );
        if matching_pending {
            let remaining = self
                .pending_remaining
                .raw()
                .saturating_sub(fill.qty.raw())
                .max(0);
            self.pending_remaining = Qty::from_raw(remaining);
            if remaining == 0 {
                self.pending = None;
            }
        }

        if fill.side == Side::Buy {
            self.entry_cost_raw_qty +=
                i128::from(fill.price.raw()) * i128::from(fill.qty.raw());
            self.entry_qty_raw += i128::from(fill.qty.raw());
        } else if fill.side == Side::Sell && !ctx.position(INSTRUMENT).is_positive() {
            self.entry_cost_raw_qty = 0;
            self.entry_qty_raw = 0;
        }
    }
}

export_strategy!(TrendConditionedPullback::new());
