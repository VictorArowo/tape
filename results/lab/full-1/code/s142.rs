use tape_guest::{
    Bar, Ctx, InstrumentId, Money, OrderRequest, Price, Qty, Side, Strategy, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const SCALE: f64 = 100_000_000.0;

#[derive(Clone, Copy)]
enum OrderKind {
    Core,
    TacticalBuy,
    TacticalSell,
}

#[derive(Clone, Copy)]
struct PendingOrder {
    kind: OrderKind,
    signal_close: Price,
    got_fill: bool,
}

struct CoreSatellite {
    core_target_raw: Option<i64>,
    core_filled_raw: i64,
    core_notional_raw: i128,
    core_qty_raw: i128,
    reference: Option<Price>,
    tactical_qty_raw: i64,
    tactical_cash: Money,
    pending: Option<PendingOrder>,
}

fn qty_for_notional(notional: f64, price: Price) -> Qty {
    if price.raw() <= 0 {
        return Qty::from_raw(0);
    }
    let raw = (notional / price.to_f64() * SCALE).floor();
    if !raw.is_finite() || raw <= 0.0 {
        Qty::from_raw(0)
    } else {
        Qty::from_raw(raw as i64)
    }
}

fn subtract_from_tactical_cash(cash: Money, amount_raw: i64) -> Money {
    Money::from_raw(cash.raw().saturating_sub(amount_raw).max(0))
}

impl CoreSatellite {
    fn submit_core(&mut self, ctx: &mut Ctx, bar: &Bar, remaining_raw: i64) {
        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        let qty_raw = affordable.raw().min(remaining_raw).max(0);
        if qty_raw <= 0 {
            return;
        }

        self.pending = Some(PendingOrder {
            kind: OrderKind::Core,
            signal_close: bar.close,
            got_fill: false,
        });
        ctx.submit(OrderRequest::market(
            INSTRUMENT,
            Side::Buy,
            Qty::from_raw(qty_raw),
        ));
    }

    fn submit_tactical_buy(&mut self, ctx: &mut Ctx, bar: &Bar) {
        let desired = qty_for_notional(500.0, bar.close).raw();
        let cap_value = (ctx.equity().to_f64() * 0.20).max(0.0);
        let cap_qty = qty_for_notional(cap_value, bar.close).raw();
        let cap_room = cap_qty.saturating_sub(self.tactical_qty_raw).max(0);
        let sleeve_cash_raw = self.tactical_cash.raw().min(ctx.cash().raw()).max(0);

        if desired <= 0 || cap_room <= 0 || sleeve_cash_raw <= 0 {
            self.reference = Some(bar.close);
            return;
        }

        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            Money::from_raw(sleeve_cash_raw),
            SIZING_MARGIN_PPM,
        );
        let qty_raw = desired.min(cap_room).min(affordable.raw()).max(0);
        if qty_raw <= 0 {
            self.reference = Some(bar.close);
            return;
        }

        self.pending = Some(PendingOrder {
            kind: OrderKind::TacticalBuy,
            signal_close: bar.close,
            got_fill: false,
        });
        ctx.submit(OrderRequest::market(
            INSTRUMENT,
            Side::Buy,
            Qty::from_raw(qty_raw),
        ));
    }

    fn submit_tactical_sell(&mut self, ctx: &mut Ctx, bar: &Bar) {
        let desired = qty_for_notional(500.0, bar.close).raw();
        let qty_raw = desired.min(self.tactical_qty_raw).max(0);
        if qty_raw <= 0 {
            self.reference = Some(bar.close);
            return;
        }

        self.pending = Some(PendingOrder {
            kind: OrderKind::TacticalSell,
            signal_close: bar.close,
            got_fill: false,
        });
        ctx.submit(OrderRequest::market(
            INSTRUMENT,
            Side::Sell,
            Qty::from_raw(qty_raw),
        ));
    }
}

impl Strategy for CoreSatellite {
    fn name(&self) -> String {
        "core-satellite-volatility-harvest".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT || bar.close.raw() <= 0 {
            return;
        }

        if self.core_target_raw.is_none() {
            let target = qty_for_notional(8_000.0, bar.close).raw().max(1);
            self.core_target_raw = Some(target);
        }

        if ctx.open_orders() == 0 {
            if let Some(pending) = self.pending.take() {
                if !pending.got_fill {
                    match pending.kind {
                        OrderKind::TacticalBuy | OrderKind::TacticalSell => {
                            self.reference = Some(pending.signal_close);
                        }
                        OrderKind::Core => {}
                    }
                }
            }
        } else {
            return;
        }

        let target_raw = self.core_target_raw.unwrap_or(0);
        let remaining_raw = target_raw.saturating_sub(self.core_filled_raw).max(0);
        if remaining_raw > 0 {
            self.submit_core(ctx, bar, remaining_raw);
            return;
        }

        let Some(reference) = self.reference else {
            return;
        };
        let close_scaled = i128::from(bar.close.raw()) * 100;
        let reference_raw = i128::from(reference.raw());

        if close_scaled >= reference_raw * 102 {
            self.submit_tactical_sell(ctx, bar);
        } else if close_scaled <= reference_raw * 98 {
            self.submit_tactical_buy(ctx, bar);
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        let kind = match self.pending.as_mut() {
            Some(pending) => {
                pending.got_fill = true;
                pending.kind
            }
            None => return,
        };

        match kind {
            OrderKind::Core => {
                let qty_raw = fill.qty.raw().max(0);
                self.core_filled_raw = self.core_filled_raw.saturating_add(qty_raw);
                self.core_qty_raw += i128::from(qty_raw);
                self.core_notional_raw +=
                    i128::from(fill.price.raw()) * i128::from(qty_raw);
                if self.core_qty_raw > 0 {
                    let vwap_raw = self.core_notional_raw / self.core_qty_raw;
                    self.reference = Some(Price::from_raw(vwap_raw as i64));
                }
                self.tactical_cash = subtract_from_tactical_cash(
                    self.tactical_cash,
                    fill.fee.raw(),
                );
            }
            OrderKind::TacticalBuy => {
                self.tactical_qty_raw = self
                    .tactical_qty_raw
                    .saturating_add(fill.qty.raw().max(0));
                let gross = fill.price * fill.qty;
                let cost_raw = gross.raw().saturating_add(fill.fee.raw());
                self.tactical_cash = subtract_from_tactical_cash(self.tactical_cash, cost_raw);
                self.reference = Some(fill.price);
            }
            OrderKind::TacticalSell => {
                self.tactical_qty_raw = self
                    .tactical_qty_raw
                    .saturating_sub(fill.qty.raw().max(0))
                    .max(0);
                let gross = fill.price * fill.qty;
                let proceeds_raw = gross.raw().saturating_sub(fill.fee.raw());
                self.tactical_cash = Money::from_raw(
                    self.tactical_cash.raw().saturating_add(proceeds_raw).max(0),
                );
                self.reference = Some(fill.price);
            }
        }
    }
}

export_strategy!(CoreSatellite {
    core_target_raw: None,
    core_filled_raw: 0,
    core_notional_raw: 0,
    core_qty_raw: 0,
    reference: None,
    tactical_qty_raw: 0,
    tactical_cash: Money::from_int(2_000).unwrap_or(Money::from_raw(0)),
    pending: None,
});