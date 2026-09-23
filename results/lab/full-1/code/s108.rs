use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const LOOKBACK: usize = 240;
const BREAKOUT_LOOKBACK: usize = 30;
const MAX_HISTORY: usize = LOOKBACK + 1;
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy)]
struct Sample {
    close_raw: i64,
    high_raw: i64,
    signed_volume: f64,
    volume: f64,
}

struct AbsorptionReclaim {
    history: VecDeque<Sample>,
    entry_price: Option<Price>,
    entry_qty_raw: i64,
    completed_bars_in_position: usize,
    exit_requested: bool,
}

impl AbsorptionReclaim {
    fn new() -> Self {
        Self {
            history: VecDeque::with_capacity(MAX_HISTORY),
            entry_price: None,
            entry_qty_raw: 0,
            completed_bars_in_position: 0,
            exit_requested: false,
        }
    }

    fn volume_cap(volume: Qty) -> Qty {
        volume.mul_ratio(1, 10)
    }

    fn min_qty(a: Qty, b: Qty) -> Qty {
        Qty::from_raw(a.raw().min(b.raw()))
    }
}

impl Strategy for AbsorptionReclaim {
    fn name(&self) -> String {
        "Absorption Reclaim".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        // The completed history here is the only information available when
        // submitting an order for the next bar.
        let preceding_high = if self.history.len() >= BREAKOUT_LOOKBACK {
            self.history
                .iter()
                .rev()
                .take(BREAKOUT_LOOKBACK)
                .map(|sample| sample.high_raw)
                .max()
        } else {
            None
        };

        let high_low = bar.high.to_f64() - bar.low.to_f64();
        let signed_volume = if high_low == 0.0 {
            0.0
        } else {
            bar.volume.to_f64()
                * (2.0 * bar.close.to_f64() - bar.high.to_f64() - bar.low.to_f64())
                / high_low
        };
        self.history.push_back(Sample {
            close_raw: bar.close.raw(),
            high_raw: bar.high.raw(),
            signed_volume,
            volume: bar.volume.to_f64(),
        });
        if self.history.len() > MAX_HISTORY {
            self.history.pop_front();
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            self.completed_bars_in_position =
                self.completed_bars_in_position.saturating_add(1);

            if let Some(entry_price) = self.entry_price {
                let stop_price = entry_price.mul_ratio(985, 1_000);
                if bar.close <= stop_price
                    || self.completed_bars_in_position >= LOOKBACK
                {
                    self.exit_requested = true;
                }
            }

            if self.exit_requested && ctx.open_orders() == 0 {
                let qty = Self::min_qty(position, Self::volume_cap(bar.volume));
                if qty.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
                }
            }
            return;
        }

        if self.exit_requested {
            self.exit_requested = false;
        }

        if !position.is_zero()
            || self.entry_qty_raw > 0
            || ctx.open_orders() != 0
            || self.history.len() < MAX_HISTORY
        {
            return;
        }

        let Some(prior_high) = preceding_high else {
            return;
        };

        let mut signed_sum = 0.0;
        let mut volume_sum = 0.0;
        for sample in self.history.iter().rev().take(LOOKBACK) {
            signed_sum += sample.signed_volume;
            volume_sum += sample.volume;
        }
        if volume_sum <= 0.0 {
            return;
        }
        let imbalance = signed_sum / volume_sum;

        let Some(reference) = self.history.front() else {
            return;
        };
        let reference_close = reference.close_raw as f64;
        if reference_close == 0.0 {
            return;
        }
        let return_240 = bar.close.raw() as f64 / reference_close - 1.0;

        let signal = imbalance >= 0.15
            && (-0.015..= -0.002).contains(&return_240)
            && bar.close.raw() > prior_high;
        if !signal || ctx.open_orders() != 0 {
            return;
        }

        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        let qty = Self::min_qty(affordable, Self::volume_cap(bar.volume));
        if qty.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.side == Side::Buy {
            let old_qty = self.entry_qty_raw;
            let fill_qty = fill.qty.raw();
            let new_qty = old_qty.saturating_add(fill_qty);
            if new_qty > 0 {
                let weighted_raw =
                    (i128::from(self.entry_price.unwrap_or(fill.price).raw())
                        * i128::from(old_qty)
                        + i128::from(fill.price.raw()) * i128::from(fill_qty))
                        / i128::from(new_qty);
                self.entry_price = Some(Price::from_raw(weighted_raw as i64));
                self.entry_qty_raw = new_qty;
            }
        } else if fill.side == Side::Sell {
            self.entry_qty_raw = self.entry_qty_raw.saturating_sub(fill.qty.raw()).max(0);
            if self.entry_qty_raw == 0 {
                self.entry_price = None;
                self.completed_bars_in_position = 0;
                self.exit_requested = false;
            }
        }
    }
}

export_strategy!(AbsorptionReclaim::new());
