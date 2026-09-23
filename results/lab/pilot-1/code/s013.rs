use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, Money, OrderRequest, Price, Qty, Side, Strategy, TimeDelta, Timestamp, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const LOOKBACK: usize = 60;
const HOLD_NANOS: i64 = TimeDelta::from_hours(12).as_nanos();
const SIZING_MARGIN_PPM: i64 = 5_000;

struct StabilizedSelloffReversal {
    // Completed-bar true ranges, closes, and volumes. The current bar is added
    // only after its signal has been evaluated, so indicators use prior bars.
    true_ranges: VecDeque<i64>,
    closes: VecDeque<i64>,
    volumes: VecDeque<i64>,
    entry_price: Option<Price>,
    entry_ts: Option<Timestamp>,
}

impl StabilizedSelloffReversal {
    fn new() -> Self {
        Self {
            true_ranges: VecDeque::with_capacity(LOOKBACK + 1),
            closes: VecDeque::with_capacity(LOOKBACK + 1),
            volumes: VecDeque::with_capacity(LOOKBACK + 1),
            entry_price: None,
            entry_ts: None,
        }
    }

    fn median_volume(&self) -> Option<i64> {
        if self.volumes.len() != LOOKBACK {
            return None;
        }
        let mut values: Vec<i64> = self.volumes.iter().copied().collect();
        values.sort_unstable();
        // For an even-sized window, use the arithmetic mean of the middle pair.
        Some(((i128::from(values[LOOKBACK / 2 - 1])
            + i128::from(values[LOOKBACK / 2])) / 2) as i64)
    }

    fn prior_atr(&self) -> Option<i64> {
        if self.true_ranges.len() != LOOKBACK {
            return None;
        }
        let sum: i128 = self.true_ranges.iter().map(|&x| i128::from(x)).sum();
        Some((sum / LOOKBACK as i128) as i64)
    }

    fn record_bar(&mut self, bar: &Bar) {
        let previous_close = self.closes.back().copied();
        let mut tr = bar.high.raw().saturating_sub(bar.low.raw());
        if let Some(previous) = previous_close {
            tr = tr.max(bar.high.raw().saturating_sub(previous).abs());
            tr = tr.max(bar.low.raw().saturating_sub(previous).abs());
        }
        self.true_ranges.push_back(tr);
        self.closes.push_back(bar.close.raw());
        self.volumes.push_back(bar.volume.raw());
        while self.true_ranges.len() > LOOKBACK {
            self.true_ranges.pop_front();
        }
        while self.closes.len() > LOOKBACK {
            self.closes.pop_front();
        }
        while self.volumes.len() > LOOKBACK {
            self.volumes.pop_front();
        }
    }
}

impl Strategy for StabilizedSelloffReversal {
    fn name(&self) -> String {
        "stabilized-selloff-reversal".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let atr = self.prior_atr();
        let median_volume = self.median_volume();
        // With the current close at the back, the oldest of the 60 prior
        // closes is the close 60 bars before this bar.
        let close_60_bars_ago = (self.closes.len() == LOOKBACK)
            .then(|| self.closes.front().copied())
            .flatten();

        if ctx.open_orders() == 0 {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                let elapsed = match self.entry_ts {
                    Some(ts) => bar.close_ts.as_nanos().saturating_sub(ts.as_nanos()) >= HOLD_NANOS,
                    None => false,
                };
                let stop = match (self.entry_price, atr) {
                    (Some(entry), Some(atr_raw)) => {
                        i128::from(bar.close.raw())
                            <= i128::from(entry.raw()) - i128::from(atr_raw)
                    }
                    _ => false,
                };
                if elapsed || stop {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                }
            } else if position.is_zero() {
                if let (Some(atr_raw), Some(median), Some(old_close)) =
                    (atr, median_volume, close_60_bars_ago)
                {
                    let selloff = i128::from(bar.close.raw()) * 2
                        <= i128::from(old_close) * 2 - i128::from(atr_raw) * 5;
                    let high_volume = i128::from(bar.volume.raw())
                        >= i128::from(median) * 2;
                    let above_midpoint = i128::from(bar.close.raw()) * 2
                        > i128::from(bar.high.raw()) + i128::from(bar.low.raw());
                    if selloff && high_volume && above_midpoint {
                        let budget = ctx.equity().mul_ratio(1, 4);
                        let affordable = ctx.affordable_qty(
                            INSTRUMENT,
                            bar.close,
                            budget,
                            SIZING_MARGIN_PPM,
                        );
                        // The next bar's volume is not available at signal time;
                        // use 10% of this completed bar's volume as the order cap.
                        let volume_cap = bar.volume.mul_ratio(1, 10);
                        let qty = if affordable < volume_cap { affordable } else { volume_cap };
                        if qty.is_positive() {
                            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                        }
                    }
                }
            }
        }

        self.record_bar(bar);
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.side == Side::Buy {
            self.entry_price = Some(fill.price);
            self.entry_ts = Some(fill.ts);
        } else if fill.side == Side::Sell {
            self.entry_price = None;
            self.entry_ts = None;
        }
    }
}

export_strategy!(StabilizedSelloffReversal::new());
