use tape_guest::{
    Bar, Ctx, Fill, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

struct RoundNumberSupportReclaim {
    previous_close_raw: Option<i64>,
    entry_price_raw: Option<i64>,
    entry_ts_nanos: Option<i64>,
    remaining_qty_raw: i64,
    exit_triggered: bool,
    cooldown_until_nanos: Option<i64>,
}

impl RoundNumberSupportReclaim {
    fn scaled(raw: i64, numerator: i64, denominator: i64) -> i64 {
        Price::from_raw(raw)
            .mul_ratio(numerator, denominator)
            .raw()
    }

    fn reclaim_signal(previous_close_raw: i64, bar: &Bar) -> bool {
        if previous_close_raw <= 0 {
            return false;
        }

        let Some(level_unit_raw) = Price::from_int(1_000).map(|price| price.raw()) else {
            return false;
        };
        let mut level_raw = previous_close_raw.div_euclid(level_unit_raw) * level_unit_raw;
        if previous_close_raw.rem_euclid(level_unit_raw) == 0 {
            level_raw -= level_unit_raw;
        }
        if level_raw <= 0 {
            return false;
        }

        let above_level = Self::scaled(level_raw, 1_002, 1_000);
        previous_close_raw > above_level
            && bar.low.raw() >= Self::scaled(level_raw, 997, 1_000)
            && bar.low.raw() <= Self::scaled(level_raw, 1_001, 1_000)
            && bar.close.raw() > above_level
    }

    fn submit_entry(&self, ctx: &mut Ctx, bar: &Bar) {
        let cash = ctx.cash();
        let equity = ctx.equity();
        let budget = if cash < equity { cash } else { equity };
        if !budget.is_positive() {
            return;
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
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }
}

impl Strategy for RoundNumberSupportReclaim {
    fn name(&self) -> String {
        "round-number-support-reclaim".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let previous_close = self.previous_close_raw;
        self.previous_close_raw = Some(bar.close.raw());

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            if !self.exit_triggered {
                if let (Some(entry_raw), Some(entry_ts)) =
                    (self.entry_price_raw, self.entry_ts_nanos)
                {
                    let close_raw = bar.close.raw();
                    let stop_raw = Self::scaled(entry_raw, 994, 1_000);
                    let target_raw = Self::scaled(entry_raw, 1_010, 1_000);
                    let elapsed = bar.close_ts.as_nanos().saturating_sub(entry_ts);
                    let time_exit = elapsed >= TimeDelta::from_hours(8).as_nanos();
                    if close_raw <= stop_raw || close_raw >= target_raw || time_exit {
                        self.exit_triggered = true;
                    }
                }
            }

            if self.exit_triggered && ctx.open_orders() == 0 {
                let volume_cap = bar.volume.mul_ratio(1, 10);
                let qty = if position < volume_cap {
                    position
                } else {
                    volume_cap
                };
                if qty.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
                }
            }
            return;
        }

        if !position.is_zero() || ctx.open_orders() != 0 {
            return;
        }

        let now = bar.close_ts.as_nanos();
        if self
            .cooldown_until_nanos
            .is_some_and(|until| now < until)
        {
            return;
        }

        if let Some(previous_close_raw) = previous_close {
            if Self::reclaim_signal(previous_close_raw, bar) {
                self.submit_entry(ctx, bar);
            }
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Buy {
            let fill_qty = fill.qty.raw();
            if self.remaining_qty_raw <= 0 {
                self.entry_price_raw = Some(fill.price.raw());
                self.entry_ts_nanos = Some(fill.ts.as_nanos());
                self.remaining_qty_raw = fill_qty;
            } else {
                let old_qty = self.remaining_qty_raw;
                let new_qty = old_qty.saturating_add(fill_qty);
                if new_qty > 0 {
                    let weighted_price = (i128::from(self.entry_price_raw.unwrap_or(fill.price.raw()))
                        * i128::from(old_qty)
                        + i128::from(fill.price.raw()) * i128::from(fill_qty))
                        / i128::from(new_qty);
                    self.entry_price_raw = Some(weighted_price as i64);
                    self.remaining_qty_raw = new_qty;
                }
            }
        } else {
            self.remaining_qty_raw = self.remaining_qty_raw.saturating_sub(fill.qty.raw());
            if self.remaining_qty_raw <= 0 {
                self.remaining_qty_raw = 0;
                self.entry_price_raw = None;
                self.entry_ts_nanos = None;
                self.exit_triggered = false;
                self.cooldown_until_nanos = Some(
                    fill.ts
                        .as_nanos()
                        .saturating_add(TimeDelta::from_hours(12).as_nanos()),
                );
            }
        }
    }
}

export_strategy!(RoundNumberSupportReclaim {
    previous_close_raw: None,
    entry_price_raw: None,
    entry_ts_nanos: None,
    remaining_qty_raw: 0,
    exit_triggered: false,
    cooldown_until_nanos: None,
});
