use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const HIGH_WINDOW: usize = 10_080;
const EMA_PERIOD: usize = 2_880;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct RollingHigh {
    next_index: u64,
    values: VecDeque<(u64, i64)>,
}

impl RollingHigh {
    fn new() -> Self {
        Self {
            next_index: 0,
            values: VecDeque::with_capacity(HIGH_WINDOW),
        }
    }

    fn push(&mut self, close_raw: i64) -> i64 {
        let index = self.next_index;
        self.next_index += 1;
        let earliest = index.saturating_sub((HIGH_WINDOW - 1) as u64);

        while let Some((old_index, _)) = self.values.front().copied() {
            if old_index < earliest {
                self.values.pop_front();
            } else {
                break;
            }
        }
        while let Some((_, old_close)) = self.values.back().copied() {
            if old_close <= close_raw {
                self.values.pop_back();
            } else {
                break;
            }
        }
        self.values.push_back((index, close_raw));
        self.values.back().map(|(_, high)| *high).unwrap_or(close_raw)
    }
}

struct Ema {
    period: usize,
    samples: usize,
    seed_sum: f64,
    value: Option<f64>,
}

impl Ema {
    fn new(period: usize) -> Self {
        Self {
            period,
            samples: 0,
            seed_sum: 0.0,
            value: None,
        }
    }

    fn push(&mut self, price: Price) {
        let close = price.to_f64();
        if let Some(previous) = self.value {
            let alpha = 2.0 / (self.period as f64 + 1.0);
            self.value = Some(previous + alpha * (close - previous));
        } else {
            self.seed_sum += close;
            self.samples += 1;
            if self.samples == self.period {
                self.value = Some(self.seed_sum / self.period as f64);
            }
        }
    }
}

struct DrawdownRebound {
    highs: RollingHigh,
    ema: Ema,
    ever_entered: bool,
    flat_low_raw: Option<i64>,
}

impl DrawdownRebound {
    fn submit_buy(&mut self, ctx: &mut Ctx, price: Price) {
        let qty = ctx.affordable_qty(
            INSTRUMENT,
            price,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        if qty.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }
}

impl Strategy for DrawdownRebound {
    fn name(&self) -> String {
        "drawdown-rebound-re-entry(10080,94%,103%,2880)".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let close_raw = bar.close.raw();
        let rolling_high = self.highs.push(close_raw);
        self.ema.push(bar.close);

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            self.ever_entered = true;
        }

        // Make the initial full-cash entry on the first available bar. Market
        // orders submitted from on_bar are executed by the sandbox at its next opportunity.
        if !self.ever_entered && position.is_zero() {
            if ctx.open_orders() == 0 {
                self.submit_buy(ctx, bar.close);
            }
            return;
        }

        if ctx.open_orders() != 0 {
            return;
        }

        if position.is_positive() {
            let at_or_below_stop = i128::from(close_raw) * 100
                <= i128::from(rolling_high) * 94;
            if at_or_below_stop {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
            return;
        }

        let low = self
            .flat_low_raw
            .map_or(close_raw, |previous| previous.min(close_raw));
        self.flat_low_raw = Some(low);

        let Some(ema_value) = self.ema.value else {
            return;
        };
        let rebound_reached = i128::from(close_raw) * 100 >= i128::from(low) * 103;
        let above_ema = bar.close.to_f64() > ema_value;
        if rebound_reached && above_ema {
            self.submit_buy(ctx, bar.close);
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.side == Side::Buy {
            self.ever_entered = true;
            self.flat_low_raw = None;
        } else {
            // If the exit fills completely, the next flat bar starts the new low.
            self.flat_low_raw = None;
        }
    }
}

export_strategy!(DrawdownRebound {
    highs: RollingHigh::new(),
    ema: Ema::new(EMA_PERIOD),
    ever_entered: false,
    flat_low_raw: None,
});
