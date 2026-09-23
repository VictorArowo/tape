use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta,
    TimeInForce, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const FOUR_HOUR_MINUTES: usize = 240;
const LOOKBACK_INTERVALS: usize = 42;
const HISTORY_CLOSES: usize = LOOKBACK_INTERVALS + 1;
const SIZING_MARGIN_PPM: i64 = 5_000;
const ENTRY_EFFICIENCY: f64 = 0.55;
const EXIT_EFFICIENCY: f64 = 0.25;

struct FourHourAggregator {
    bucket: Option<i64>,
    count: usize,
    contiguous: bool,
    previous_ts: Option<i64>,
}

impl FourHourAggregator {
    fn new() -> Self {
        Self {
            bucket: None,
            count: 0,
            contiguous: true,
            previous_ts: None,
        }
    }

    fn push(&mut self, bar: &Bar) -> Option<Price> {
        let ts = bar.close_ts.as_nanos();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let four_hour_ns = TimeDelta::from_hours(4).as_nanos();
        if minute_ns <= 0 || four_hour_ns <= 0 {
            return None;
        }

        // A bar timestamp marks its close, so an exact boundary belongs to the
        // four-hour bucket that just ended.
        let bucket = ts.saturating_sub(1).div_euclid(four_hour_ns);
        if self.bucket != Some(bucket) {
            self.bucket = Some(bucket);
            self.count = 0;
            self.contiguous = true;
            self.previous_ts = None;
        }

        if let Some(previous) = self.previous_ts {
            if ts.saturating_sub(previous) != minute_ns {
                self.contiguous = false;
            }
        }
        self.previous_ts = Some(ts);
        self.count += 1;

        let is_boundary = ts.rem_euclid(four_hour_ns) == 0;
        if is_boundary && self.count == FOUR_HOUR_MINUTES && self.contiguous {
            Some(bar.close)
        } else {
            None
        }
    }
}

struct PathHistory {
    closes: VecDeque<Price>,
}

impl PathHistory {
    fn new() -> Self {
        Self {
            closes: VecDeque::with_capacity(HISTORY_CLOSES),
        }
    }

    fn push(&mut self, close: Price) -> Option<(f64, f64)> {
        if !close.is_positive() {
            return None;
        }
        self.closes.push_back(close);
        if self.closes.len() > HISTORY_CLOSES {
            self.closes.pop_front();
        }
        if self.closes.len() != HISTORY_CLOSES {
            return None;
        }

        let first = self.closes.front()?.to_f64();
        let last = self.closes.back()?.to_f64();
        if first <= 0.0 || last <= 0.0 {
            return None;
        }

        let log_return = (last / first).ln();
        let mut path_length = 0.0;
        for pair in self.closes.as_slices().0.windows(2) {
            let a = pair[0].to_f64();
            let b = pair[1].to_f64();
            if a > 0.0 && b > 0.0 {
                path_length += (b / a).ln().abs();
            }
        }
        // VecDeque can wrap, so include adjacent pairs across its two slices.
        let (first_slice, second_slice) = self.closes.as_slices();
        if !first_slice.is_empty() && !second_slice.is_empty() {
            let a = first_slice[first_slice.len() - 1].to_f64();
            let b = second_slice[0].to_f64();
            if a > 0.0 && b > 0.0 {
                path_length += (b / a).ln().abs();
            }
            for pair in second_slice.windows(2) {
                let a = pair[0].to_f64();
                let b = pair[1].to_f64();
                if a > 0.0 && b > 0.0 {
                    path_length += (b / a).ln().abs();
                }
            }
        }

        let efficiency = if path_length > 0.0 {
            log_return.abs() / path_length
        } else {
            0.0
        };
        Some((log_return, efficiency))
    }
}

struct EfficientPathDrift {
    aggregate: FourHourAggregator,
    history: PathHistory,
    pending_active: bool,
    pending_buy: bool,
    pending_qty: Qty,
}

impl EfficientPathDrift {
    fn set_pending(&mut self, buy: bool, qty: Qty) {
        if qty.is_positive() {
            self.pending_active = true;
            self.pending_buy = buy;
            self.pending_qty = qty;
        } else {
            self.clear_pending();
        }
    }

    fn clear_pending(&mut self) {
        self.pending_active = false;
        self.pending_qty = Qty::from_raw(0);
    }

    fn try_submit(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if !self.pending_active || ctx.open_orders() != 0 {
            return;
        }

        let mut remaining = self.pending_qty;
        if !self.pending_buy {
            let position = ctx.position(INSTRUMENT);
            if !position.is_positive() {
                self.clear_pending();
                return;
            }
            if position.raw() < remaining.raw() {
                remaining = position;
            }
        }

        // Use the latest completed minute's volume as the per-attempt cap.
        // The execution venue/sandbox should additionally apply its exact cap
        // to the bar in which the next-open order is executed.
        let volume_cap = Qty::from_raw((bar.volume.raw() / 10).max(0));
        let mut qty = Qty::from_raw(remaining.raw().min(volume_cap.raw()).max(0));
        if !qty.is_positive() {
            return;
        }

        if self.pending_buy {
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if affordable.raw() < qty.raw() {
                qty = affordable;
            }
            if !qty.is_positive() {
                return;
            }
            ctx.submit(
                OrderRequest::market(INSTRUMENT, Side::Buy, qty)
                    .with_time_in_force(TimeInForce::Ioc),
            );
        } else {
            ctx.submit(
                OrderRequest::market(INSTRUMENT, Side::Sell, qty)
                    .with_time_in_force(TimeInForce::Ioc),
            );
        }
    }
}

impl Strategy for EfficientPathDrift {
    fn name(&self) -> String {
        String::from("efficient-path-drift")
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        if let Some(four_hour_close) = self.aggregate.push(bar) {
            if let Some((log_return, efficiency)) = self.history.push(four_hour_close) {
                if ctx.open_orders() == 0 {
                    let position = ctx.position(INSTRUMENT);
                    if position.is_positive()
                        && (log_return <= 0.0 || efficiency <= EXIT_EFFICIENCY)
                    {
                        self.set_pending(false, position);
                    } else if position.is_zero()
                        && log_return > 0.0
                        && efficiency >= ENTRY_EFFICIENCY
                        && !self.pending_active
                    {
                        let target_budget = ctx.equity().mul_ratio(99, 100);
                        let target_qty = ctx.affordable_qty(
                            INSTRUMENT,
                            bar.close,
                            target_budget,
                            SIZING_MARGIN_PPM,
                        );
                        self.set_pending(true, target_qty);
                    }
                }
            }
        }

        // Market IOC orders are issued after the signal bar closes; any
        // unfilled desired quantity is retried on a later bar after the prior
        // order has finished.
        self.try_submit(ctx, bar);
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if !self.pending_active {
            return;
        }
        let matches_pending = if self.pending_buy {
            fill.side == Side::Buy
        } else {
            fill.side == Side::Sell
        };
        if !matches_pending {
            return;
        }

        let left = self
            .pending_qty
            .raw()
            .saturating_sub(fill.qty.raw())
            .max(0);
        self.pending_qty = Qty::from_raw(left);
        if left == 0 {
            self.clear_pending();
        }
    }
}

export_strategy!(EfficientPathDrift {
    aggregate: FourHourAggregator::new(),
    history: PathHistory::new(),
    pending_active: false,
    pending_buy: false,
    pending_qty: Qty::from_raw(0),
});
