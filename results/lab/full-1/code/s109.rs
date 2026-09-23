use std::collections::HashSet;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const FEE_MARGIN_PPM: i64 = 5_000;
const HOLD_LIMIT_BARS: u16 = 720;

struct RoundNumberDefense {
    previous_close: Option<Price>,
    disarmed_levels: HashSet<i64>,
    pending_entry_level: Option<i64>,
    active_level: Option<i64>,
    bars_held: u16,
    exiting: bool,
}

impl RoundNumberDefense {
    fn handle_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        let step = Price::from_int(1_000).expect("valid level size").raw();
        let rearm_distance = Price::from_int(250).expect("valid re-arm distance").raw();
        let close_raw = bar.close.raw();

        self.disarmed_levels.retain(|level| {
            i128::from(close_raw) < i128::from(*level) + i128::from(rearm_distance)
        });

        let position = ctx.position(INSTRUMENT);

        // Reconcile state in case a fill notification was not delivered before this bar.
        if self.active_level.is_none() && position.is_positive() {
            if let Some(level) = self.pending_entry_level.take() {
                self.active_level = Some(level);
                self.bars_held = 0;
            }
        }
        if position.is_zero() {
            self.active_level = None;
            self.bars_held = 0;
            self.exiting = false;
            if self.pending_entry_level.is_some() && ctx.open_orders() == 0 {
                self.pending_entry_level = None;
            }
        }

        if position.is_positive() {
            let Some(level) = self.active_level else {
                return;
            };

            self.bars_held = self.bars_held.saturating_add(1);
            let target = i128::from(level) + i128::from(step);
            let stop = i128::from(level) * 997 / 1_000;
            let close = i128::from(close_raw);
            if close >= target || close <= stop || self.bars_held >= HOLD_LIMIT_BARS {
                self.exiting = true;
            }

            if self.exiting && ctx.open_orders() == 0 {
                // Market orders are IOC; any volume-capped remainder is retried on a later bar.
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
            return;
        }

        if ctx.open_orders() != 0 || self.pending_entry_level.is_some() {
            return;
        }

        let Some(previous_close) = self.previous_close else {
            return;
        };
        let level = (i128::from(previous_close.raw()).div_euclid(i128::from(step))
            * i128::from(step)) as i64;

        if self.disarmed_levels.contains(&level) {
            return;
        }

        let swept = i128::from(bar.low.raw()) <= i128::from(level) - 100_000_000;
        let recovered = i128::from(close_raw) >= i128::from(level) + 50_000_000;
        let previous_above = i128::from(previous_close.raw()) > i128::from(level);
        let range = i128::from(bar.high.raw()) - i128::from(bar.low.raw());
        let close_in_top_quarter =
            4 * (i128::from(close_raw) - i128::from(bar.low.raw())) >= 3 * range;

        if !(swept && recovered && previous_above && close_in_top_quarter) {
            return;
        }

        let budget = ctx.cash().mul_ratio(95, 100);
        let qty = ctx.affordable_qty(INSTRUMENT, bar.close, budget, FEE_MARGIN_PPM);
        if qty.is_positive() {
            self.disarmed_levels.insert(level);
            self.pending_entry_level = Some(level);
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }
}

impl Strategy for RoundNumberDefense {
    fn name(&self) -> String {
        "round-number-defense".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }
        self.handle_bar(ctx, bar);
        self.previous_close = Some(bar.close);
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.side == Side::Buy && fill.qty.is_positive() {
            if self.active_level.is_none() {
                self.active_level = self.pending_entry_level.take();
                self.bars_held = 0;
            }
        } else if fill.side == Side::Sell && ctx.position(INSTRUMENT).is_zero() {
            self.active_level = None;
            self.pending_entry_level = None;
            self.bars_held = 0;
            self.exiting = false;
        }
    }
}

export_strategy!(RoundNumberDefense {
    previous_close: None,
    disarmed_levels: HashSet::new(),
    pending_entry_level: None,
    active_level: None,
    bars_held: 0,
    exiting: false,
});
