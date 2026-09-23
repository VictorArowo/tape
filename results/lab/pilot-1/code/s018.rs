use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, Timestamp, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const LOOKBACK_BARS: usize = 1_440;
const PAUSE_NANOS: i64 = TimeDelta::from_hours(24).as_nanos();
const SIZING_MARGIN_PPM: i64 = 5_000;
const EXHAUSTION_RETURN: f64 = 0.12;

struct RallyExhaustionPause {
    closes: VecDeque<i64>,
    exit_triggered: bool,
    last_exit_fill: Option<Timestamp>,
    pause_until: Option<i64>,
}

impl RallyExhaustionPause {
    fn new() -> Self {
        Self {
            closes: VecDeque::with_capacity(LOOKBACK_BARS + 1),
            exit_triggered: false,
            last_exit_fill: None,
            pause_until: None,
        }
    }
}

impl Strategy for RallyExhaustionPause {
    fn name(&self) -> String {
        "Rally Exhaustion Pause".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        self.closes.push_back(bar.close.raw());
        if self.closes.len() > LOOKBACK_BARS + 1 {
            self.closes.pop_front();
        }

        if ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            if let Some(oldest) = self.closes.front().copied().filter(|_| self.closes.len() == LOOKBACK_BARS + 1) {
                let old = oldest as f64;
                let current = bar.close.raw() as f64;
                if old > 0.0 && current / old - 1.0 > EXHAUSTION_RETURN {
                    self.exit_triggered = true;
                }
            }

            if self.exit_triggered {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
            return;
        }

        if self.exit_triggered {
            if let Some(exit_fill) = self.last_exit_fill {
                self.pause_until = Some(exit_fill.as_nanos().saturating_add(PAUSE_NANOS));
                self.last_exit_fill = None;
                self.exit_triggered = false;
            } else {
                return;
            }
        }

        if let Some(until) = self.pause_until {
            if bar.close_ts.as_nanos() < until {
                return;
            }
            let qty = ctx.affordable_qty(INSTRUMENT, bar.close, ctx.cash(), SIZING_MARGIN_PPM);
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                self.pause_until = None;
            }
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.side == Side::Sell && self.exit_triggered {
            self.last_exit_fill = Some(fill.ts);
        }
    }
}

export_strategy!(RallyExhaustionPause::new());
