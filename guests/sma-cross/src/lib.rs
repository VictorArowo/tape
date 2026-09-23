//! The reference moving-average crossover from `tape-strategies`, written
//! against the sandbox SDK. Given the same data and settings it must
//! produce exactly the same orders and fills as the native version.

use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const FAST: usize = 5;
const SLOW: usize = 20;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct Sma {
    len: usize,
    window: VecDeque<i64>,
    sum: i128,
}

impl Sma {
    fn new(len: usize) -> Self {
        Self {
            len,
            window: VecDeque::with_capacity(len + 1),
            sum: 0,
        }
    }

    fn push(&mut self, price: Price) {
        self.window.push_back(price.raw());
        self.sum += i128::from(price.raw());
        if self.window.len() > self.len {
            self.sum -= i128::from(self.window.pop_front().expect("non-empty"));
        }
    }

    fn ready_sum(&self) -> Option<i128> {
        (self.window.len() == self.len).then_some(self.sum)
    }
}

struct SmaCross {
    fast: Sma,
    slow: Sma,
}

impl Strategy for SmaCross {
    fn name(&self) -> String {
        format!("sma-cross({FAST},{SLOW}) [wasm]")
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }
        self.fast.push(bar.close);
        self.slow.push(bar.close);
        let (Some(fast), Some(slow)) = (self.fast.ready_sum(), self.slow.ready_sum()) else {
            return;
        };
        if ctx.open_orders() > 0 {
            return;
        }
        let want_long = fast * SLOW as i128 > slow * FAST as i128;
        let position = ctx.position(INSTRUMENT);
        if want_long && position.is_zero() {
            let qty = ctx.affordable_qty(INSTRUMENT, bar.close, ctx.cash(), SIZING_MARGIN_PPM);
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        } else if !want_long && position.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
        }
    }
}

export_strategy!(SmaCross {
    fast: Sma::new(FAST),
    slow: Sma::new(SLOW),
});
