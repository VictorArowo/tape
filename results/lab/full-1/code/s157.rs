use tape_guest::{
    Bar, Ctx, Fill, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

struct RoundNumberSweepReclaim {
    previous_30m_close: Option<Price>,
    entry_pending: bool,
    pending_level: Option<Price>,
    entry_price: Option<Price>,
    entry_qty: Qty,
    entry_ts_ns: Option<i64>,
    entry_level: Option<Price>,
    exit_triggered: bool,
    exit_order_pending: bool,
}

impl RoundNumberSweepReclaim {
    fn new() -> Self {
        Self {
            previous_30m_close: None,
            entry_pending: false,
            pending_level: None,
            entry_price: None,
            entry_qty: Qty::from_raw(0),
            entry_ts_ns: None,
            entry_level: None,
            exit_triggered: false,
            exit_order_pending: false,
        }
    }

    fn clear_position_state(&mut self) {
        self.entry_price = None;
        self.entry_qty = Qty::from_raw(0);
        self.entry_ts_ns = None;
        self.entry_level = None;
        self.exit_triggered = false;
        self.exit_order_pending = false;
        self.pending_level = None;
        self.entry_pending = false;
    }

    fn reclaimed_level(&self, bar: &Bar) -> Option<Price> {
        let previous_close = self.previous_30m_close?;
        let step = Price::from_int(2_500).expect("positive round-number step");

        // The relevant candidate is the highest round level strictly below
        // both closes. If its sweep threshold was not reached, lower levels
        // cannot have been swept either.
        let upper = bar
            .close
            .raw()
            .saturating_sub(1)
            .min(previous_close.raw().saturating_sub(1));
        if upper <= 0 {
            return None;
        }

        let level_raw = upper.div_euclid(step.raw()) * step.raw();
        if level_raw <= 0 {
            return None;
        }

        let level = Price::from_raw(level_raw);
        let sweep_threshold = level.mul_ratio(998, 1_000);
        (bar.low <= sweep_threshold).then_some(level)
    }

    fn process_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if ctx.open_orders() == 0 {
            if self.entry_pending {
                self.entry_pending = false;
                self.pending_level = None;
            }
            self.exit_order_pending = false;
        } else {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            let timed_out = self.entry_ts_ns.is_some_and(|entry_ts| {
                bar.close_ts
                    .as_nanos()
                    .saturating_sub(entry_ts)
                    >= TimeDelta::from_hours(48).as_nanos()
            });
            let price_exit = self.entry_price.is_some_and(|entry| {
                bar.close >= entry.mul_ratio(102, 100)
                    || bar.close <= entry.mul_ratio(99, 100)
            });
            let level_exit = self
                .entry_level
                .is_some_and(|level| bar.close < level);

            if price_exit || level_exit || timed_out {
                self.exit_triggered = true;
            }

            if self.exit_triggered {
                let volume_cap = bar.volume.mul_ratio(1, 10);
                let qty = if position < volume_cap {
                    position
                } else {
                    volume_cap
                };
                if qty.is_positive() {
                    self.exit_order_pending = true;
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
                }
            }
            return;
        }

        if self.entry_pending {
            return;
        }
        self.clear_position_state();

        let Some(level) = self.reclaimed_level(bar) else {
            return;
        };

        let cash = ctx.cash();
        let mut budget = ctx.equity().mul_ratio(1, 2);
        if !cash.is_positive() || !budget.is_positive() {
            return;
        }
        if cash < budget {
            budget = cash;
        }

        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            budget,
            SIZING_MARGIN_PPM,
        );
        let volume_cap = bar.volume.mul_ratio(1, 10);
        let qty = if affordable < volume_cap {
            affordable
        } else {
            volume_cap
        };

        if qty.is_positive() {
            self.pending_level = Some(level);
            self.entry_pending = true;
            // A market order submitted after this bar closes is intended to
            // execute at the following bar's open.
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }
}

impl Strategy for RoundNumberSweepReclaim {
    fn name(&self) -> String {
        String::from("round-number-sweep-reclaim")
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        // Process against the previously observed 30-minute close before
        // recording this bar if it is itself a 30-minute boundary.
        self.process_bar(ctx, bar);

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let minute_index = bar.close_ts.as_nanos() / minute_ns;
        if minute_index.rem_euclid(30) == 0 {
            self.previous_30m_close = Some(bar.close);
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Buy {
            if self.entry_qty.is_positive() {
                let old_qty = i128::from(self.entry_qty.raw());
                let added_qty = i128::from(fill.qty.raw());
                let total_qty = old_qty + added_qty;
                let old_cost = i128::from(self.entry_price.unwrap_or(fill.price).raw()) * old_qty;
                let added_cost = i128::from(fill.price.raw()) * added_qty;
                self.entry_price = Some(Price::from_raw(((old_cost + added_cost) / total_qty) as i64));
                self.entry_qty = Qty::from_raw(total_qty as i64);
            } else {
                self.entry_price = Some(fill.price);
                self.entry_qty = fill.qty;
                self.entry_ts_ns = Some(fill.ts.as_nanos());
                self.entry_level = self.pending_level;
            }
            self.entry_pending = false;
            self.pending_level = None;
        } else if ctx.position(INSTRUMENT).is_zero() {
            self.clear_position_state();
        }
    }
}

export_strategy!(RoundNumberSweepReclaim::new());
