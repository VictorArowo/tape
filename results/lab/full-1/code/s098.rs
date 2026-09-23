use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const HISTORY_DAYS: usize = 20;
const BREAKOUT_DAYS: usize = 10;
const EXIT_LOOKBACK_DAYS: usize = 5;
const STALE_HIGH_DAYS: i64 = 5;
const MAX_HOLD_DAYS: i64 = 15;
const VOLUME_CAP_NUM: i64 = 1;
const VOLUME_CAP_DEN: i64 = 10;
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy)]
struct DailyBar {
    day: i64,
    close_raw: i64,
    low_raw: i64,
}

#[derive(Clone, Copy)]
struct BuildingDay {
    day: i64,
    close_raw: i64,
    low_raw: i64,
}

#[derive(Clone, Copy)]
enum IntentSide {
    Buy,
    Sell,
}

struct PendingOrder {
    side: IntentSide,
    remaining: Qty,
}

struct StaleHighSupplyClearance {
    history: VecDeque<DailyBar>,
    current_day: Option<BuildingDay>,
    pending: Option<PendingOrder>,
    order_in_flight: bool,
    entry_day: Option<i64>,
}

impl StaleHighSupplyClearance {
    fn finalize_day(&mut self, ctx: &mut Ctx) {
        let Some(today) = self.current_day.take() else {
            return;
        };

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            let stop_hit = self.history.len() >= EXIT_LOOKBACK_DAYS
                && self
                    .history
                    .iter()
                    .rev()
                    .take(EXIT_LOOKBACK_DAYS)
                    .all(|day| today.close_raw < day.low_raw);
            let time_exit = self
                .entry_day
                .is_some_and(|entry_day| today.day - entry_day >= MAX_HOLD_DAYS);

            if stop_hit || time_exit {
                self.pending = Some(PendingOrder {
                    side: IntentSide::Sell,
                    remaining: position,
                });
            }
        } else if position.is_zero() && self.pending.is_none() && self.history.len() >= HISTORY_DAYS {
            let previous_10_high = self
                .history
                .iter()
                .rev()
                .take(BREAKOUT_DAYS)
                .map(|day| day.close_raw)
                .max();
            let previous_20_high = self
                .history
                .iter()
                .rev()
                .take(HISTORY_DAYS)
                .map(|day| day.close_raw)
                .max();

            if let (Some(high_10), Some(high_20)) = (previous_10_high, previous_20_high) {
                let most_recent_20_high_day = self
                    .history
                    .iter()
                    .rev()
                    .take(HISTORY_DAYS)
                    .find(|day| day.close_raw == high_20)
                    .map(|day| day.day);

                let stale_high = most_recent_20_high_day
                    .is_some_and(|high_day| today.day - high_day >= STALE_HIGH_DAYS);

                if today.close_raw > high_10 && stale_high {
                    let price = Price::from_raw(today.close_raw);
                    let qty = ctx.affordable_qty(
                        INSTRUMENT,
                        price,
                        ctx.cash(),
                        SIZING_MARGIN_PPM,
                    );
                    if qty.is_positive() {
                        self.pending = Some(PendingOrder {
                            side: IntentSide::Buy,
                            remaining: qty,
                        });
                    }
                }
            }
        }

        self.history.push_back(DailyBar {
            day: today.day,
            close_raw: today.close_raw,
            low_raw: today.low_raw,
        });
        while self.history.len() > HISTORY_DAYS {
            self.history.pop_front();
        }
    }

    fn try_submit(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if self.order_in_flight {
            if ctx.open_orders() > 0 {
                return;
            }
            self.order_in_flight = false;
        }
        if ctx.open_orders() != 0 {
            return;
        }

        let Some(pending) = self.pending.as_ref() else {
            return;
        };
        let volume_cap = bar.volume.mul_ratio(VOLUME_CAP_NUM, VOLUME_CAP_DEN);
        if !volume_cap.is_positive() {
            return;
        }

        let (side, qty) = match pending.side {
            IntentSide::Buy => {
                let affordable = ctx.affordable_qty(
                    INSTRUMENT,
                    bar.close,
                    ctx.cash(),
                    SIZING_MARGIN_PPM,
                );
                let raw = pending
                    .remaining
                    .raw()
                    .min(volume_cap.raw())
                    .min(affordable.raw());
                (Side::Buy, Qty::from_raw(raw))
            }
            IntentSide::Sell => {
                let held = ctx.position(INSTRUMENT);
                let raw = pending
                    .remaining
                    .raw()
                    .min(volume_cap.raw())
                    .min(held.raw());
                (Side::Sell, Qty::from_raw(raw))
            }
        };

        if !qty.is_positive() {
            return;
        }

        let _ = ctx.submit(OrderRequest::market(INSTRUMENT, side, qty));
        self.order_in_flight = true;
    }
}

impl Strategy for StaleHighSupplyClearance {
    fn name(&self) -> String {
        "stale-high-supply-clearance".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let day_nanos = TimeDelta::from_days(1).as_nanos();
        if day_nanos <= 0 {
            return;
        }
        let day = bar.close_ts.as_nanos().div_euclid(day_nanos);

        if self
            .current_day
            .is_some_and(|building| building.day != day)
        {
            // The prior UTC daily bar is complete. Its close alone forms the signal;
            // the newly observed day's bar is not used in that calculation.
            self.finalize_day(ctx);
        }

        match self.current_day.as_mut() {
            Some(building) => {
                building.close_raw = bar.close.raw();
                building.low_raw = building.low_raw.min(bar.low.raw());
            }
            None => {
                self.current_day = Some(BuildingDay {
                    day,
                    close_raw: bar.close.raw(),
                    low_raw: bar.low.raw(),
                });
            }
        }

        // IOC orders are retried only after the previous order has finished.
        // Each attempt is limited to ten percent of the most recently observed bar volume.
        self.try_submit(ctx, bar);
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        match fill.side {
            Side::Buy => {
                self.entry_day.get_or_insert_with(|| {
                    let day_nanos = TimeDelta::from_days(1).as_nanos();
                    fill.ts.as_nanos().div_euclid(day_nanos)
                });
                if let Some(pending) = self.pending.as_mut() {
                    if matches!(pending.side, IntentSide::Buy) {
                        pending.remaining = Qty::from_raw(
                            pending.remaining.raw().saturating_sub(fill.qty.raw()).max(0),
                        );
                        if pending.remaining.is_zero() {
                            self.pending = None;
                        }
                    }
                }
            }
            Side::Sell => {
                if let Some(pending) = self.pending.as_mut() {
                    if matches!(pending.side, IntentSide::Sell) {
                        pending.remaining = Qty::from_raw(
                            pending.remaining.raw().saturating_sub(fill.qty.raw()).max(0),
                        );
                        if pending.remaining.is_zero() {
                            self.pending = None;
                        }
                    }
                }
                if ctx.position(INSTRUMENT).is_zero() {
                    self.entry_day = None;
                }
            }
        }
    }
}

export_strategy!(StaleHighSupplyClearance {
    history: VecDeque::with_capacity(HISTORY_DAYS + 1),
    current_day: None,
    pending: None,
    order_in_flight: false,
    entry_day: None,
});
