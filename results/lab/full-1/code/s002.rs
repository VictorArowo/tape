use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const RANGE_HIGH_BARS: usize = 6;
const EXIT_LOW_BARS: usize = 2;
const NO_LEVERAGE_MARGIN_PPM: i64 = 1_000_000;
const BUY_SLIPPAGE_FACTOR_NUM: i64 = 1_000_200;
const BUY_SLIPPAGE_FACTOR_DEN: i64 = 1_000_000;

struct FourHourBar {
    bucket: i64,
    open: Price,
    high: Price,
    low: Price,
    close: Price,
    starts_at_boundary: bool,
}

struct RangeBreakout {
    current: Option<FourHourBar>,
    prior_highs: VecDeque<i64>,
    prior_lows: VecDeque<i64>,
}

impl RangeBreakout {
    fn new() -> Self {
        Self {
            current: None,
            prior_highs: VecDeque::with_capacity(RANGE_HIGH_BARS),
            prior_lows: VecDeque::with_capacity(EXIT_LOW_BARS),
        }
    }

    fn on_four_hour_close(&mut self, ctx: &mut Ctx, completed: FourHourBar) {
        if completed.starts_at_boundary {
            let highest_prior = if self.prior_highs.len() == RANGE_HIGH_BARS {
                self.prior_highs.iter().copied().max()
            } else {
                None
            };
            let lowest_prior = if self.prior_lows.len() == EXIT_LOW_BARS {
                self.prior_lows.iter().copied().min()
            } else {
                None
            };

            if ctx.open_orders() == 0 {
                let position = ctx.position(INSTRUMENT);
                if position.is_positive() {
                    if let Some(lowest) = lowest_prior {
                        if completed.close.raw() < lowest {
                            ctx.submit(OrderRequest::market(
                                INSTRUMENT,
                                Side::Sell,
                                position,
                            ));
                        }
                    }
                } else if position.is_zero() {
                    if let Some(highest) = highest_prior {
                        if completed.close.raw() > highest
                            && completed.close > completed.open
                        {
                            // Reserve for the specified adverse buy slippage; the sandbox's
                            // affordability calculation accounts for fees.
                            let sizing_price = completed.close.mul_ratio(
                                BUY_SLIPPAGE_FACTOR_NUM,
                                BUY_SLIPPAGE_FACTOR_DEN,
                            );
                            let qty = ctx.affordable_qty(
                                INSTRUMENT,
                                sizing_price,
                                ctx.cash(),
                                NO_LEVERAGE_MARGIN_PPM,
                            );
                            if qty.is_positive() {
                                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                            }
                        }
                    }
                }
            }

            self.prior_highs.push_back(completed.high.raw());
            if self.prior_highs.len() > RANGE_HIGH_BARS {
                self.prior_highs.pop_front();
            }
            self.prior_lows.push_back(completed.low.raw());
            if self.prior_lows.len() > EXIT_LOW_BARS {
                self.prior_lows.pop_front();
            }
        }
    }
}

impl Strategy for RangeBreakout {
    fn name(&self) -> String {
        "4h Range Breakout".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let period = TimeDelta::from_hours(4).as_nanos();
        let open_ns = bar.open_ts.as_nanos();
        let close_ns = bar.close_ts.as_nanos();
        let bucket = open_ns.div_euclid(period);

        if self
            .current
            .as_ref()
            .is_some_and(|current| current.bucket != bucket)
        {
            // A bucket that did not receive a bar closing at its UTC boundary is incomplete.
            self.current = None;
        }

        if self.current.is_none() {
            self.current = Some(FourHourBar {
                bucket,
                open: bar.open,
                high: bar.high,
                low: bar.low,
                close: bar.close,
                starts_at_boundary: open_ns.rem_euclid(period) == 0,
            });
        } else if let Some(current) = self.current.as_mut() {
            if bar.high > current.high {
                current.high = bar.high;
            }
            if bar.low < current.low {
                current.low = bar.low;
            }
            current.close = bar.close;
        }

        if close_ns.div_euclid(period) > bucket {
            if let Some(completed) = self.current.take() {
                self.on_four_hour_close(ctx, completed);
            }
        }
    }
}

export_strategy!(RangeBreakout::new());
