use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, Fill, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta, Timestamp, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const LOOKBACK_BARS: usize = 60;
const SIGNAL_MINUTE_UTC: i64 = 1_380;
const ENTRY_RETURN_NUM: i64 = 1_008;
const ENTRY_RETURN_DEN: i64 = 1_000;
const STOP_NUM: i64 = 985;
const STOP_DEN: i64 = 1_000;
const VOLUME_CAP_NUM: i64 = 1;
const VOLUME_CAP_DEN: i64 = 10;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct UtcCloseReversal {
    prior_closes: VecDeque<i64>,
    entry_reference: Option<Price>,
    entry_fill_ns: Option<i64>,
    order_pending: bool,
}

impl UtcCloseReversal {
    fn new() -> Self {
        Self {
            prior_closes: VecDeque::with_capacity(LOOKBACK_BARS),
            entry_reference: None,
            entry_fill_ns: None,
            order_pending: false,
        }
    }
}

impl Strategy for UtcCloseReversal {
    fn name(&self) -> String {
        "UTC Close Reversal".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        // Market IOC orders should complete immediately. If one did not, do not
        // keep treating it as pending after the SDK reports no open orders.
        if ctx.open_orders() == 0 {
            self.order_pending = false;
        }

        let prior_close = if self.prior_closes.len() == LOOKBACK_BARS {
            self.prior_closes.front().copied()
        } else {
            None
        };

        let position = ctx.position(INSTRUMENT);
        if !self.order_pending && ctx.open_orders() == 0 {
            if position.is_positive() {
                if let Some(entry) = self.entry_reference {
                    let stopped = i128::from(bar.close.raw()) * i128::from(STOP_DEN)
                        <= i128::from(entry.raw()) * i128::from(STOP_NUM);
                    let held_long_enough = self.entry_fill_ns.is_some_and(|entry_ns| {
                        bar.close_ts.as_nanos() - entry_ns
                            >= TimeDelta::from_hours(6).as_nanos()
                    });
                    if stopped || held_long_enough {
                        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                        self.order_pending = true;
                    }
                }
            } else if bar.close_ts.as_nanos() / TimeDelta::from_mins(1).as_nanos() % 1_440
                == SIGNAL_MINUTE_UTC
            {
                if let Some(prior) = prior_close {
                    let return_triggered = i128::from(bar.close.raw())
                        * i128::from(ENTRY_RETURN_DEN)
                        >= i128::from(prior) * i128::from(ENTRY_RETURN_NUM);
                    if return_triggered {
                        let volume_cap = bar.volume.mul_ratio(VOLUME_CAP_NUM, VOLUME_CAP_DEN);
                        let affordable = ctx.affordable_qty(
                            INSTRUMENT,
                            bar.close,
                            ctx.cash(),
                            SIZING_MARGIN_PPM,
                        );
                        let qty = if volume_cap < affordable {
                            volume_cap
                        } else {
                            affordable
                        };
                        if qty.is_positive() {
                            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                            self.order_pending = true;
                        }
                    }
                }
            }
        }

        self.prior_closes.push_back(bar.close.raw());
        if self.prior_closes.len() > LOOKBACK_BARS {
            self.prior_closes.pop_front();
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Buy {
            self.entry_reference = Some(fill.price);
            self.entry_fill_ns = Some(fill.ts.as_nanos());
            self.order_pending = false;
        } else {
            self.entry_reference = None;
            self.entry_fill_ns = None;
            self.order_pending = false;
        }
    }
}

export_strategy!(UtcCloseReversal::new());
