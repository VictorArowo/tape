use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, Money, OrderRequest, Side, Strategy, TimeDelta, Timestamp,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const LOOKBACK_BARS: usize = 1_440;
const MIN_COOLDOWN: TimeDelta = TimeDelta::from_days(7);
const HOLD_PERIOD: TimeDelta = TimeDelta::from_days(7);
const SIZING_MARGIN_PPM: i64 = 5_000;

struct PostShockCooldown {
    closes: VecDeque<i64>,
    last_entry_signal: Option<Timestamp>,
    entry_fill_ts: Option<Timestamp>,
}

impl PostShockCooldown {
    fn new() -> Self {
        Self {
            closes: VecDeque::with_capacity(LOOKBACK_BARS + 1),
            last_entry_signal: None,
            entry_fill_ts: None,
        }
    }
}

impl Strategy for PostShockCooldown {
    fn name(&self) -> String {
        "post-shock-cooldown".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let reference_close = if self.closes.len() == LOOKBACK_BARS {
            self.closes.front().copied()
        } else {
            None
        };
        self.closes.push_back(bar.close.raw());
        if self.closes.len() > LOOKBACK_BARS {
            self.closes.pop_front();
        }

        if ctx.open_orders() > 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            if let Some(entry_ts) = self.entry_fill_ts {
                if bar.close_ts.as_nanos() - entry_ts.as_nanos() >= HOLD_PERIOD.as_nanos() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                }
            }
            return;
        }

        self.entry_fill_ts = None;
        let Some(old_close) = reference_close else {
            return;
        };
        if old_close <= 0 || bar.close.raw() <= 0 {
            return;
        }

        // Compare the close-to-close return with -10% without floating-point rounding.
        if i128::from(bar.close.raw()) * 10 > i128::from(old_close) * 9 {
            return;
        }
        if let Some(previous) = self.last_entry_signal {
            if bar.close_ts.as_nanos() - previous.as_nanos() < MIN_COOLDOWN.as_nanos() {
                return;
            }
        }

        // The submitted market order is executed at the next bar's open. The sandbox
        // applies fill fees, slippage, and its next-bar volume participation limit.
        let quarter_equity = ctx.equity().mul_ratio(1, 4);
        let cash = ctx.cash();
        let budget = if cash < quarter_equity { cash } else { quarter_equity };
        if !budget.is_positive() {
            return;
        }
        let qty = ctx.affordable_qty(INSTRUMENT, bar.close, budget, SIZING_MARGIN_PPM);
        if qty.is_positive() {
            self.last_entry_signal = Some(bar.close_ts);
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.order_id == fill.order_id && fill.side == Side::Buy {
            self.entry_fill_ts = Some(fill.ts);
        } else if fill.side == Side::Sell {
            self.entry_fill_ts = None;
        }
    }
}

export_strategy!(PostShockCooldown::new());
