use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const ATR_BARS: usize = 1_440;
const RANGE_BARS: usize = 4_320;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct Atr {
    window: VecDeque<i64>,
    sum: i128,
    previous_close: Option<i64>,
}

impl Atr {
    fn new() -> Self {
        Self {
            window: VecDeque::with_capacity(ATR_BARS + 1),
            sum: 0,
            previous_close: None,
        }
    }

    fn push(&mut self, bar: &Bar) -> Option<f64> {
        let high = i128::from(bar.high.raw());
        let low = i128::from(bar.low.raw());
        let mut true_range = high - low;

        if let Some(previous_close) = self.previous_close {
            let previous = i128::from(previous_close);
            true_range = true_range
                .max((high - previous).abs())
                .max((low - previous).abs());
        }

        let true_range = i64::try_from(true_range).unwrap_or(i64::MAX);
        self.window.push_back(true_range);
        self.sum += i128::from(true_range);
        if self.window.len() > ATR_BARS {
            self.sum -= i128::from(self.window.pop_front().expect("non-empty ATR window"));
        }
        self.previous_close = Some(bar.close.raw());

        (self.window.len() == ATR_BARS)
            .then(|| self.sum as f64 / ATR_BARS as f64)
    }
}

struct Setup {
    level: Price,
    armed_at_ns: i64,
}

struct RangeAcceptanceRetest {
    atr: Atr,
    highs: VecDeque<(u64, i64)>,
    bar_index: u64,
    setup: Option<Setup>,
    pending_entry_level: Option<Price>,
    active_level: Option<Price>,
    entered_at_ns: Option<i64>,
    pending_exit: bool,
}

impl RangeAcceptanceRetest {
    fn new() -> Self {
        Self {
            atr: Atr::new(),
            highs: VecDeque::with_capacity(RANGE_BARS + 1),
            bar_index: 0,
            setup: None,
            pending_entry_level: None,
            active_level: None,
            entered_at_ns: None,
            pending_exit: false,
        }
    }

    fn clear_trade(&mut self) {
        self.pending_entry_level = None;
        self.active_level = None;
        self.entered_at_ns = None;
        self.pending_exit = false;
    }
}

impl Strategy for RangeAcceptanceRetest {
    fn name(&self) -> String {
        "range-acceptance-retest".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let now_ns = bar.close_ts.as_nanos();
        let atr_raw = self.atr.push(bar);

        // Keep only the highs of the preceding 72 hours; the current bar is
        // deliberately added after evaluating today's breakout condition.
        while let Some(&(index, _)) = self.highs.front() {
            if index + RANGE_BARS as u64 <= self.bar_index {
                self.highs.pop_front();
            } else {
                break;
            }
        }
        let range_high = (self.bar_index >= RANGE_BARS as u64)
            .then(|| self.highs.front().map(|&(_, high)| high))
            .flatten();

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            if self.active_level.is_none() {
                self.active_level = self.pending_entry_level;
                self.entered_at_ns = Some(now_ns);
                self.pending_entry_level = None;
            }

            if ctx.open_orders() == 0 && !self.pending_exit {
                if let (Some(level), Some(atr_raw)) = (self.active_level, atr_raw) {
                    let close_broke_down =
                        (bar.close.raw() as f64) < (level.raw() as f64 - 0.5 * atr_raw);
                    let time_expired = self
                        .entered_at_ns
                        .map(|entered| {
                            now_ns - entered >= TimeDelta::from_days(7).as_nanos()
                        })
                        .unwrap_or(false);

                    if close_broke_down || time_expired {
                        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                        self.pending_exit = true;
                    }
                }
            }
        } else {
            if self.active_level.is_some() || self.pending_exit {
                self.clear_trade();
            }
            if ctx.open_orders() == 0 && self.pending_entry_level.is_some() {
                // An IOC entry that did not fill has finished.
                self.pending_entry_level = None;
            }

            if ctx.open_orders() == 0 && self.pending_entry_level.is_none() {
                let mut accepted_level = None;

                if let Some(setup) = self.setup.as_ref() {
                    let elapsed = now_ns - setup.armed_at_ns;
                    let setup_expired = elapsed > TimeDelta::from_hours(12).as_nanos();
                    if setup_expired {
                        self.setup = None;
                    } else if elapsed > 0 {
                        if let Some(atr_raw) = atr_raw {
                            let level = setup.level.raw() as f64;
                            let low = bar.low.raw() as f64;
                            let close = bar.close.raw() as f64;
                            if low <= level + 0.25 * atr_raw
                                && low >= level - 0.5 * atr_raw
                                && close > level
                            {
                                accepted_level = Some(setup.level);
                            }
                        }
                    }
                }

                if let Some(level) = accepted_level {
                    self.setup = None;
                    let affordable = ctx.affordable_qty(
                        INSTRUMENT,
                        bar.close,
                        ctx.cash(),
                        SIZING_MARGIN_PPM,
                    );
                    // Use the completed signal bar's available volume as a
                    // conservative order-size cap; the simulator applies its
                    // own execution volume cap as well.
                    let qty = if affordable.raw() < bar.volume.raw() {
                        affordable
                    } else {
                        bar.volume
                    };
                    if qty.is_positive() {
                        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                        self.pending_entry_level = Some(level);
                    }
                } else if self.setup.is_none() {
                    if let (Some(high), Some(atr_raw)) = (range_high, atr_raw) {
                        let breakout_threshold =
                            high as f64 + 0.1 * atr_raw;
                        if bar.close.raw() as f64 >= breakout_threshold {
                            self.setup = Some(Setup {
                                level: Price::from_raw(high),
                                armed_at_ns: now_ns,
                            });
                        }
                    }
                }
            }
        }

        while let Some(&(_, high)) = self.highs.back() {
            if high <= bar.high.raw() {
                self.highs.pop_back();
            } else {
                break;
            }
        }
        self.highs.push_back((self.bar_index, bar.high.raw()));
        self.bar_index += 1;
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.side == Side::Buy {
            if self.active_level.is_none() {
                self.active_level = self.pending_entry_level;
            }
            if self.entered_at_ns.is_none() {
                self.entered_at_ns = Some(fill.ts.as_nanos());
            }
        }
    }
}

export_strategy!(RangeAcceptanceRetest::new());
