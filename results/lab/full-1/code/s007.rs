use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, Fill, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, Timestamp,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const ROLLING_LEN: usize = 30;
const BASELINE_LEN: usize = 360;
const ROLLING_HISTORY_LEN: usize = BASELINE_LEN + ROLLING_LEN;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct SelloffAbsorptionReversal {
    closes: VecDeque<Price>,
    lows: VecDeque<i64>,
    volumes: VecDeque<i64>,
    rolling_volume: i128,
    rolling_sums: VecDeque<i128>,
    previous_signal: bool,
    entry_qty_raw: i128,
    entry_price_raw: i64,
    entry_ts: Option<Timestamp>,
    exit_triggered: bool,
}

impl SelloffAbsorptionReversal {
    fn new() -> Self {
        Self {
            closes: VecDeque::with_capacity(31),
            lows: VecDeque::with_capacity(ROLLING_LEN),
            volumes: VecDeque::with_capacity(ROLLING_LEN),
            rolling_volume: 0,
            rolling_sums: VecDeque::with_capacity(ROLLING_HISTORY_LEN),
            previous_signal: false,
            entry_qty_raw: 0,
            entry_price_raw: 0,
            entry_ts: None,
            exit_triggered: false,
        }
    }

    fn update_history(&mut self, bar: &Bar) {
        self.closes.push_back(bar.close);
        if self.closes.len() > 31 {
            self.closes.pop_front();
        }

        self.lows.push_back(bar.low.raw());
        if self.lows.len() > ROLLING_LEN {
            self.lows.pop_front();
        }

        let volume_raw = bar.volume.raw();
        self.volumes.push_back(volume_raw);
        self.rolling_volume += i128::from(volume_raw);
        if self.volumes.len() > ROLLING_LEN {
            if let Some(old_volume) = self.volumes.pop_front() {
                self.rolling_volume -= i128::from(old_volume);
            }
        }

        if self.volumes.len() == ROLLING_LEN {
            self.rolling_sums.push_back(self.rolling_volume);
            if self.rolling_sums.len() > ROLLING_HISTORY_LEN {
                self.rolling_sums.pop_front();
            }
        }
    }

    fn signal(&self, close: Price) -> bool {
        if self.closes.len() < 31
            || self.lows.len() != ROLLING_LEN
            || self.rolling_sums.len() < ROLLING_HISTORY_LEN
        {
            return false;
        }

        let Some(old_close) = self.closes.front() else {
            return false;
        };
        let lowest_low = self.lows.iter().copied().min().unwrap_or(i64::MAX);

        // The first 360 sums in this 390-sum history end at t-389 through t-30.
        let mut baseline: Vec<i128> = self
            .rolling_sums
            .iter()
            .take(BASELINE_LEN)
            .copied()
            .collect();
        if baseline.len() != BASELINE_LEN {
            return false;
        }
        baseline.sort_unstable();
        let median_pair_sum = baseline[BASELINE_LEN / 2 - 1] + baseline[BASELINE_LEN / 2];

        let return_is_low =
            i128::from(close.raw()) * 1_000 <= i128::from(old_close.raw()) * 988;
        let rebound_from_low =
            i128::from(close.raw()) * 2_000 >= i128::from(lowest_low) * 2_007;
        let volume_is_high = 4 * self.rolling_volume >= 3 * median_pair_sum;

        return_is_low && rebound_from_low && volume_is_high
    }

    fn record_fill(&mut self, fill: &Fill) {
        let qty = i128::from(fill.qty.raw());
        if qty <= 0 {
            return;
        }

        if fill.side == Side::Buy {
            if self.entry_qty_raw == 0 {
                self.entry_price_raw = fill.price.raw();
                self.entry_ts = Some(fill.ts);
            } else {
                let new_qty = self.entry_qty_raw + qty;
                let weighted_price = i128::from(self.entry_price_raw) * self.entry_qty_raw
                    + i128::from(fill.price.raw()) * qty;
                self.entry_price_raw = (weighted_price / new_qty) as i64;
            }
            self.entry_qty_raw += qty;
        } else {
            self.entry_qty_raw -= qty;
            if self.entry_qty_raw <= 0 {
                self.entry_qty_raw = 0;
                self.entry_price_raw = 0;
                self.entry_ts = None;
                self.exit_triggered = false;
            }
        }
    }
}

impl Strategy for SelloffAbsorptionReversal {
    fn name(&self) -> String {
        "selloff-absorption-reversal".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        self.update_history(bar);
        let signal = self.signal(bar.close);
        let new_signal = signal && !self.previous_signal;
        self.previous_signal = signal;

        let position = ctx.position(INSTRUMENT);
        if position.is_zero() {
            self.exit_triggered = false;
        } else if position.is_positive() {
            let profit_exit = i128::from(bar.close.raw()) * 200
                >= i128::from(self.entry_price_raw) * 203;
            let stop_exit = i128::from(bar.close.raw()) * 100
                <= i128::from(self.entry_price_raw) * 99;
            let time_exit = self.entry_ts.is_some_and(|entry_ts| {
                i128::from(bar.close_ts.as_nanos()) - i128::from(entry_ts.as_nanos())
                    >= i128::from(TimeDelta::from_mins(180).as_nanos())
            });
            if profit_exit || stop_exit || time_exit {
                self.exit_triggered = true;
            }
        }

        if ctx.open_orders() != 0 {
            return;
        }

        if position.is_positive() {
            if self.exit_triggered {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
            return;
        }

        if new_signal {
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &Fill) {
        self.record_fill(fill);
    }
}

export_strategy!(SelloffAbsorptionReversal::new());
