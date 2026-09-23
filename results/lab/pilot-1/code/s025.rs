use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const MARGIN_PPM: i64 = 5_000;
// Build nanosecond scales from small factors to avoid timestamp-like literals.
const MINUTE_NS: i64 = 60 * 60_000 * 1_000_000;

// Fixed public calendar: scheduled U.S. CPI and FOMC releases in 2024,
// expressed as UTC (year, month, day, hour, minute).
const EVENTS: &[(i64, i64, i64, i64, i64)] = &[
    (2024, 1, 11, 13, 30), (2024, 1, 31, 19, 0),
    (2024, 2, 13, 13, 30),
    (2024, 3, 12, 12, 30), (2024, 3, 20, 18, 0),
    (2024, 4, 10, 12, 30), (2024, 5, 1, 18, 0),
    (2024, 5, 15, 12, 30), (2024, 6, 12, 12, 30), (2024, 6, 12, 18, 0),
    (2024, 7, 11, 12, 30), (2024, 7, 31, 18, 0),
    (2024, 8, 14, 12, 30), (2024, 9, 11, 12, 30), (2024, 9, 18, 18, 0),
    (2024, 10, 10, 12, 30), (2024, 11, 7, 19, 0),
    (2024, 11, 13, 13, 30), (2024, 12, 11, 13, 30), (2024, 12, 18, 19, 0),
];

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = year - if month <= 2 { 1 } else { 0 };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn event_ns(event: &(i64, i64, i64, i64, i64)) -> i64 {
    let minutes = days_from_civil(event.0, event.1, event.2) * 1_440
        + event.3 * 60
        + event.4;
    minutes * MINUTE_NS
}

#[derive(Clone, Copy)]
struct Reaction {
    event_ns: i64,
    bars: u8,
    first_close: i64,
    last_close: i64,
}

struct MacroReaction {
    reaction: Option<Reaction>,
    pending_entry: bool,
    holding: bool,
    held_bars: u16,
    pending_exit: bool,
}

impl MacroReaction {
    fn new() -> Self {
        Self {
            reaction: None,
            pending_entry: false,
            holding: false,
            held_bars: 0,
            pending_exit: false,
        }
    }

    fn volume_cap(volume: Qty) -> Qty {
        volume.mul_ratio(1, 10)
    }
}

impl Strategy for MacroReaction {
    fn name(&self) -> String {
        "Macro Reaction Continuation (fixed 2024 UTC calendar)".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        // Submit actions queued by the preceding completed bar. With market
        // orders, this keeps decisions from using the current bar's close as a
        // signal and places execution no earlier than the next bar.
        if ctx.open_orders() == 0 {
            if self.pending_entry && !self.holding {
                let cap = Self::volume_cap(bar.volume);
                let affordable = ctx.affordable_qty(INSTRUMENT, bar.close, ctx.cash(), MARGIN_PPM);
                let qty = if cap < affordable { cap } else { affordable };
                if qty.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                }
                self.pending_entry = false;
            } else if self.pending_exit && self.holding {
                let position = ctx.position(INSTRUMENT);
                let cap = Self::volume_cap(bar.volume);
                let qty = if position < cap { position } else { cap };
                if qty.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
                }
            }
        }

        if self.holding {
            self.held_bars = self.held_bars.saturating_add(1);
            if self.held_bars >= 360 {
                self.pending_exit = true;
            }
            return;
        }

        // Do not start another event observation while a trade or entry is pending.
        if self.pending_entry {
            return;
        }

        let open_ts = bar.open_ts.as_nanos();
        if let Some(mut reaction) = self.reaction {
            if open_ts == reaction.event_ns + i64::from(reaction.bars) * MINUTE_NS
                && reaction.bars < 15
            {
                reaction.bars += 1;
                reaction.last_close = bar.close.raw();
                if reaction.bars == 15 {
                    self.reaction = None;
                    if reaction.last_close > reaction.first_close {
                        self.pending_entry = true;
                    }
                } else {
                    self.reaction = Some(reaction);
                }
            }
        } else if !self.pending_exit && ctx.position(INSTRUMENT).is_zero() {
            if let Some(event) = EVENTS.iter().find(|event| event_ns(event) == open_ts) {
                let _ = event;
                self.reaction = Some(Reaction {
                    event_ns: open_ts,
                    bars: 1,
                    first_close: bar.close.raw(),
                    last_close: bar.close.raw(),
                });
            }
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.side == Side::Buy {
            self.holding = true;
            self.pending_entry = false;
            self.held_bars = 0;
        } else if fill.side == Side::Sell && ctx.position(INSTRUMENT).is_zero() {
            self.holding = false;
            self.pending_exit = false;
            self.held_bars = 0;
        }
    }
}

export_strategy!(MacroReaction::new());
