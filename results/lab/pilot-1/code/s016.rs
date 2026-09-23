use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, Fill, InstrumentId, OrderRequest, Qty, Side, Strategy, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const TURNOVER_WINDOW: usize = 60;
const MEDIAN_WINDOW: usize = 1_440;
const HISTORY_NEEDED: usize = MEDIAN_WINDOW + TURNOVER_WINDOW;
const MAX_HOLD_BARS: usize = 360;
const SIZING_MARGIN_PPM: i64 = 5_000;
const SCALE: f64 = 100_000_000.0;

struct TurnoverAbsorptionConfirmation {
    turnovers: VecDeque<f64>,
    closes: VecDeque<i64>,
    entry_price: Option<f64>,
    bars_in_position: usize,
}

impl TurnoverAbsorptionConfirmation {
    fn new() -> Self {
        Self {
            turnovers: VecDeque::with_capacity(HISTORY_NEEDED),
            closes: VecDeque::with_capacity(61),
            entry_price: None,
            bars_in_position: 0,
        }
    }

    fn turnover(bar: &Bar) -> f64 {
        (bar.close * bar.volume).raw() as f64
    }

    fn median(values: &[f64]) -> f64 {
        let mut sorted = values.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let middle = sorted.len() / 2;
        if sorted.len() % 2 == 0 {
            (sorted[middle - 1] + sorted[middle]) * 0.5
        } else {
            sorted[middle]
        }
    }

    fn price_f64(raw: i64) -> f64 {
        raw as f64 / SCALE
    }

    fn capped_qty(qty: Qty, volume: Qty) -> Qty {
        let cap_raw = (i128::from(volume.raw()) / 10)
            .max(0)
            .min(i128::from(i64::MAX)) as i64;
        Qty::from_raw(qty.raw().min(cap_raw))
    }
}

impl Strategy for TurnoverAbsorptionConfirmation {
    fn name(&self) -> String {
        "Turnover Absorption Confirmation".to_owned()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let current_turnover = Self::turnover(bar);
        let signal = if self.turnovers.len() >= MEDIAN_WINDOW && self.closes.len() >= 60 {
            // The stored turnover observations are all prior completed bars.
            let prior_start = self.turnovers.len() - MEDIAN_WINDOW;
            let prior: Vec<f64> = self.turnovers.iter().skip(prior_start).copied().collect();
            let median = Self::median(&prior);
            let turnover_60 = self
                .turnovers
                .iter()
                .rev()
                .take(TURNOVER_WINDOW - 1)
                .copied()
                .sum::<f64>()
                + current_turnover;
            let close_now = bar.close.to_f64();
            let close_60 = Self::price_f64(self.closes[self.closes.len() - 60]);
            let close_5 = Self::price_f64(self.closes[self.closes.len() - 5]);
            let return_60 = close_now / close_60 - 1.0;
            turnover_60 >= 2.0 * median * TURNOVER_WINDOW as f64
                && return_60.abs() <= 0.005
                && close_now >= close_5 * 1.0015
        } else {
            false
        };

        if ctx.position(INSTRUMENT).is_positive() {
            self.bars_in_position = self.bars_in_position.saturating_add(1);
            let should_exit = self.entry_price.is_some_and(|entry| {
                let change = bar.close.to_f64() / entry - 1.0;
                self.bars_in_position >= MAX_HOLD_BARS || change <= -0.015 || change >= 0.03
            });
            if should_exit && ctx.open_orders() == 0 {
                let position = ctx.position(INSTRUMENT);
                if position.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                }
            }
        } else {
            self.entry_price = None;
            self.bars_in_position = 0;
            if signal && ctx.open_orders() == 0 {
                let affordable = ctx.affordable_qty(
                    INSTRUMENT,
                    bar.close,
                    ctx.cash(),
                    SIZING_MARGIN_PPM,
                );
                let qty = Self::capped_qty(affordable, bar.volume);
                if qty.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                }
            }
        }

        self.turnovers.push_back(current_turnover);
        if self.turnovers.len() > HISTORY_NEEDED {
            self.turnovers.pop_front();
        }
        self.closes.push_back(bar.close.raw());
        if self.closes.len() > 61 {
            self.closes.pop_front();
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Buy {
            self.entry_price = Some(fill.price.to_f64());
            self.bars_in_position = 0;
        } else if fill.side == Side::Sell {
            self.entry_price = None;
            self.bars_in_position = 0;
        }
    }
}

export_strategy!(TurnoverAbsorptionConfirmation::new());
