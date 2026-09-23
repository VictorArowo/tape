use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const RETURN_BARS: usize = 1_440;
const HIGH_BARS: usize = 720;
const EMA_BARS: f64 = 2_880.0;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct ShockExitReclaim {
    closes: VecDeque<i64>,
    ema: Option<f64>,
    started: bool,
    initializing: bool,
    exit_active: bool,
    entry_active: bool,
}

impl ShockExitReclaim {
    fn new() -> Self {
        Self {
            closes: VecDeque::with_capacity(RETURN_BARS + 1),
            ema: None,
            started: false,
            initializing: false,
            exit_active: false,
            entry_active: false,
        }
    }

    fn volume_cap(bar: &Bar) -> Qty {
        bar.volume.mul_ratio(1, 10)
    }

    fn buy_qty(ctx: &Ctx, bar: &Bar, reference_price: Price) -> Qty {
        // Reserve for the stated 0.02% slippage; affordable_qty accounts for fees.
        let estimated_price = reference_price.mul_ratio(10_002, 10_000);
        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            estimated_price,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        let cap = Self::volume_cap(bar);
        if affordable < cap { affordable } else { cap }
    }
}

impl Strategy for ShockExitReclaim {
    fn name(&self) -> String {
        "Shock Exit, Reclaim Entry".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let first_bar = !self.started;
        if first_bar {
            self.started = true;
            self.initializing = true;
        }

        let prior_high = if self.closes.len() >= HIGH_BARS {
            self.closes
                .iter()
                .rev()
                .take(HIGH_BARS)
                .copied()
                .max()
        } else {
            None
        };
        let return_is_shock = self.closes.front().and_then(|&old_raw| {
            if self.closes.len() < RETURN_BARS || old_raw <= 0 {
                None
            } else {
                let ret = (bar.close.raw() as f64 - old_raw as f64) / old_raw as f64;
                Some(ret <= -0.04)
            }
        }).unwrap_or(false);

        let close_f64 = bar.close.to_f64();
        let alpha = 2.0 / (EMA_BARS + 1.0);
        let ema = match self.ema {
            Some(previous) => previous + alpha * (close_f64 - previous),
            None => close_f64,
        };
        self.ema = Some(ema);
        let reclaim = prior_high
            .map(|high| bar.close.raw() > high && close_f64 > ema)
            .unwrap_or(false);

        self.closes.push_back(bar.close.raw());
        if self.closes.len() > RETURN_BARS {
            self.closes.pop_front();
        }

        if self.initializing {
            if ctx.open_orders() > 0 {
                return;
            }
            let reference = if first_bar { bar.open } else { bar.close };
            let qty = Self::buy_qty(ctx, bar, reference);
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                return;
            }
            if Self::volume_cap(bar).is_zero() {
                return;
            }
            self.initializing = false;
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() && return_is_shock {
            self.exit_active = true;
            self.entry_active = false;
        }

        if self.exit_active {
            if position.is_zero() {
                self.exit_active = false;
            } else {
                if ctx.open_orders() > 0 {
                    return;
                }
                let cap = Self::volume_cap(bar);
                let qty = if position < cap { position } else { cap };
                if qty.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
                }
                return;
            }
        }

        if ctx.open_orders() > 0 {
            return;
        }

        if self.entry_active {
            let qty = Self::buy_qty(ctx, bar, bar.close);
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                return;
            }
            if Self::volume_cap(bar).is_zero() {
                return;
            }
            self.entry_active = false;
        }

        if position.is_positive() {
            return;
        }

        if reclaim {
            let qty = Self::buy_qty(ctx, bar, bar.close);
            if qty.is_positive() {
                self.entry_active = true;
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        }
    }
}

export_strategy!(ShockExitReclaim::new());
