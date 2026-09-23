use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const LOOKBACK_BARS: usize = 1_440;
const FAILURE_WINDOW: usize = 180;
const RECLAIM_WINDOW: usize = 360;
const MAX_HOLD_BARS: usize = 720;
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy)]
enum Setup {
    None,
    Armed { level: Price, age: usize },
    Failed { level: Price, age: usize },
}

struct FailedBreakoutReclaim {
    highs: VecDeque<i64>,
    setup: Setup,
    pending_entry: bool,
    pending_level: Option<Price>,
    active_level: Option<Price>,
    pending_exit: bool,
    held_bars: usize,
}

impl FailedBreakoutReclaim {
    fn new() -> Self {
        Self {
            highs: VecDeque::with_capacity(LOOKBACK_BARS + 1),
            setup: Setup::None,
            pending_entry: false,
            pending_level: None,
            active_level: None,
            pending_exit: false,
            held_bars: 0,
        }
    }

    fn record_high(&mut self, high: Price) {
        self.highs.push_back(high.raw());
        if self.highs.len() > LOOKBACK_BARS {
            self.highs.pop_front();
        }
    }
}

impl Strategy for FailedBreakoutReclaim {
    fn name(&self) -> String {
        "Failed Breakout Reclaim".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        // Take the reference level before adding this bar, so the current bar is excluded.
        let prior_high = if self.highs.len() == LOOKBACK_BARS {
            self.highs.iter().copied().max().map(Price::from_raw)
        } else {
            None
        };
        self.record_high(bar.high);

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            if self.pending_entry {
                self.pending_entry = false;
                if self.active_level.is_none() {
                    self.active_level = self.pending_level.take();
                } else {
                    self.pending_level = None;
                }
            }

            self.held_bars = self.held_bars.saturating_add(1);
            let level_exit = self
                .active_level
                .is_some_and(|level| bar.close <= level.mul_ratio(995, 1_000));
            let time_exit = self.held_bars >= MAX_HOLD_BARS;

            if self.pending_exit {
                if ctx.open_orders() > 0 {
                    return;
                }
                self.pending_exit = false;
            }

            if (level_exit || time_exit) && ctx.open_orders() == 0 {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                self.pending_exit = true;
            }
            return;
        }

        if self.pending_exit {
            self.pending_exit = false;
            self.active_level = None;
            self.held_bars = 0;
            self.setup = Setup::None;
        }

        if self.pending_entry {
            if ctx.open_orders() > 0 {
                return;
            }
            // The IOC entry did not result in a position.
            self.pending_entry = false;
            self.pending_level = None;
        }

        match self.setup {
            Setup::None => {
                if let Some(level) = prior_high {
                    if bar.close > level {
                        self.setup = Setup::Armed { level, age: 0 };
                    }
                }
            }
            Setup::Armed { level, age } => {
                let next_age = age.saturating_add(1);
                if next_age <= FAILURE_WINDOW
                    && bar.close <= level.mul_ratio(997, 1_000)
                {
                    self.setup = Setup::Failed { level, age: 0 };
                } else if next_age >= FAILURE_WINDOW {
                    self.setup = Setup::None;
                } else {
                    self.setup = Setup::Armed {
                        level,
                        age: next_age,
                    };
                }
            }
            Setup::Failed { level, age } => {
                let next_age = age.saturating_add(1);
                if next_age <= RECLAIM_WINDOW
                    && bar.close >= level.mul_ratio(1_001, 1_000)
                {
                    self.setup = Setup::None;
                    if ctx.open_orders() == 0 {
                        let affordable = ctx.affordable_qty(
                            INSTRUMENT,
                            bar.close,
                            ctx.cash(),
                            SIZING_MARGIN_PPM,
                        );
                        let volume_cap = bar.volume.mul_ratio(1, 10);
                        let qty = if affordable < volume_cap {
                            affordable
                        } else {
                            volume_cap
                        };
                        if qty.is_positive() {
                            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                            self.pending_entry = true;
                            self.pending_level = Some(level);
                        }
                    }
                } else if next_age >= RECLAIM_WINDOW {
                    self.setup = Setup::None;
                } else {
                    self.setup = Setup::Failed {
                        level,
                        age: next_age,
                    };
                }
            }
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        match fill.side {
            Side::Buy => {
                self.pending_entry = false;
                self.active_level = self.pending_level.take();
                self.pending_exit = false;
                self.held_bars = 0;
                self.setup = Setup::None;
            }
            Side::Sell => {
                if ctx.position(INSTRUMENT).is_zero() {
                    self.pending_exit = false;
                    self.pending_entry = false;
                    self.pending_level = None;
                    self.active_level = None;
                    self.held_bars = 0;
                    self.setup = Setup::None;
                }
            }
        }
    }
}

export_strategy!(FailedBreakoutReclaim::new());
