use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, TimeInForce, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const WINDOW: usize = 1_440;
const MAX_HOLD_BARS: u32 = 360;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct RollingVwap {
    window: VecDeque<(i128, i64)>,
    weighted_sum: i128,
    volume_sum: i128,
}

impl RollingVwap {
    fn new() -> Self {
        Self {
            window: VecDeque::with_capacity(WINDOW + 1),
            weighted_sum: 0,
            volume_sum: 0,
        }
    }

    fn push(&mut self, bar: &Bar) {
        let volume = bar.volume.raw().max(0);
        let typical_price_times_three = i128::from(bar.high.raw())
            + i128::from(bar.low.raw())
            + i128::from(bar.close.raw());

        self.window.push_back((typical_price_times_three, volume));
        self.weighted_sum += typical_price_times_three * i128::from(volume);
        self.volume_sum += i128::from(volume);

        if self.window.len() > WINDOW {
            if let Some((price_times_three, old_volume)) = self.window.pop_front() {
                self.weighted_sum -= price_times_three * i128::from(old_volume);
                self.volume_sum -= i128::from(old_volume);
            }
        }
    }

    fn value(&self) -> Option<Price> {
        if self.window.len() != WINDOW || self.volume_sum <= 0 {
            return None;
        }
        let raw = self.weighted_sum / (3 * self.volume_sum);
        i64::try_from(raw).ok().map(Price::from_raw)
    }
}

struct RollingVwapReversion {
    vwap: RollingVwap,
    entry_qty_raw: i128,
    entry_notional: i128,
    entry_price: Option<i64>,
    bars_held: u32,
}

impl RollingVwapReversion {
    fn clear_entry(&mut self) {
        self.entry_qty_raw = 0;
        self.entry_notional = 0;
        self.entry_price = None;
        self.bars_held = 0;
    }
}

impl Strategy for RollingVwapReversion {
    fn name(&self) -> String {
        "rolling-vwap-reversion(1440)".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        self.vwap.push(bar);
        let position = ctx.position(INSTRUMENT);

        if position.is_positive() {
            if self.entry_qty_raw > 0 {
                self.bars_held = self.bars_held.saturating_add(1);
            }
        } else if self.entry_qty_raw > 0 {
            self.clear_entry();
        }

        if ctx.open_orders() != 0 {
            return;
        }

        if position.is_positive() {
            let vwap_exit = self
                .vwap
                .value()
                .is_some_and(|vwap| bar.close >= vwap);
            let stop_exit = self.entry_price.is_some_and(|entry| {
                i128::from(bar.close.raw()) * 10_000 <= i128::from(entry) * 9_850
            });
            let time_exit = self.entry_qty_raw > 0 && self.bars_held >= MAX_HOLD_BARS;

            if vwap_exit || stop_exit || time_exit {
                ctx.submit(
                    OrderRequest::market(INSTRUMENT, Side::Sell, position)
                        .with_time_in_force(TimeInForce::Ioc),
                );
            }
            return;
        }

        let Some(vwap) = self.vwap.value() else {
            return;
        };
        let entry_signal =
            i128::from(bar.close.raw()) * 10_000 <= i128::from(vwap.raw()) * 9_875;
        if !entry_signal {
            return;
        }

        let budget = ctx.cash().mul_ratio(95, 100);
        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            budget,
            SIZING_MARGIN_PPM,
        );
        let volume_cap = bar.volume.mul_ratio(1, 10);
        let qty = if affordable.raw() < volume_cap.raw() {
            affordable
        } else {
            volume_cap
        };

        if qty.is_positive() {
            ctx.submit(
                OrderRequest::market(INSTRUMENT, Side::Buy, qty)
                    .with_time_in_force(TimeInForce::Ioc),
            );
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        match fill.side {
            Side::Buy => {
                let qty = i128::from(fill.qty.raw().max(0));
                if qty == 0 {
                    return;
                }
                if self.entry_qty_raw == 0 {
                    self.bars_held = 0;
                }
                self.entry_qty_raw += qty;
                self.entry_notional += i128::from(fill.price.raw()) * qty;
                let average_raw = self.entry_notional / self.entry_qty_raw;
                self.entry_price = i64::try_from(average_raw).ok();
            }
            Side::Sell => {
                self.entry_qty_raw -= i128::from(fill.qty.raw().max(0));
                if self.entry_qty_raw <= 0 {
                    self.clear_entry();
                }
            }
        }
    }
}

export_strategy!(RollingVwapReversion {
    vwap: RollingVwap::new(),
    entry_qty_raw: 0,
    entry_notional: 0,
    entry_price: None,
    bars_held: 0,
});
