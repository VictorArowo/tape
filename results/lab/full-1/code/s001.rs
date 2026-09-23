use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const ENTRY_LOOKBACK: usize = 1_440;
const EXIT_LOOKBACK: usize = 720;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct Breakout {
    // History contains only completed bars preceding the bar being evaluated.
    highs_lows: VecDeque<(i64, i64)>,
    exiting: bool,
}

impl Strategy for Breakout {
    fn name(&self) -> String {
        String::from("24h Breakout, 12h Exit")
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let history_len = self.highs_lows.len();
        let entry_level = if history_len >= ENTRY_LOOKBACK {
            self.highs_lows.iter().map(|item| item.0).max()
        } else {
            None
        };
        let exit_level = if history_len >= EXIT_LOOKBACK {
            self.highs_lows
                .iter()
                .rev()
                .take(EXIT_LOOKBACK)
                .map(|item| item.1)
                .min()
        } else {
            None
        };

        // Market orders are submitted after this bar closes and execute on the
        // following bar. IOC orders leave no entry remainder to manage.
        if ctx.open_orders() == 0 {
            let position = ctx.position(INSTRUMENT);

            if self.exiting {
                if position.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                } else {
                    self.exiting = false;
                }
            }

            if !self.exiting {
                if position.is_positive() {
                    if let Some(low) = exit_level {
                        if bar.close.raw() < low {
                            self.exiting = true;
                            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                        }
                    }
                } else if position.is_zero() {
                    if let Some(high) = entry_level {
                        let breakout = i128::from(bar.close.raw()) * 1_000
                            > i128::from(high) * 1_003;
                        if breakout {
                            // Reserve 0.02% for slippage, and size against 95%
                            // of cash. affordable_qty accounts for purchase fees.
                            let reserved_price = Price::from_raw(
                                (i128::from(bar.close.raw()) * 10_002 / 10_000) as i64,
                            );
                            let budget = ctx.cash().mul_ratio(95, 100);
                            let qty = ctx.affordable_qty(
                                INSTRUMENT,
                                reserved_price,
                                budget,
                                SIZING_MARGIN_PPM,
                            );
                            if qty.is_positive() {
                                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                            }
                        }
                    }
                }
            }
        }

        self.highs_lows
            .push_back((bar.high.raw(), bar.low.raw()));
        while self.highs_lows.len() > ENTRY_LOOKBACK {
            self.highs_lows.pop_front();
        }
    }
}

export_strategy!(Breakout {
    highs_lows: VecDeque::with_capacity(ENTRY_LOOKBACK),
    exiting: false,
});
