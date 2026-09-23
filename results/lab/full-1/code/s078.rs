use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, Fill, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta,
    TimeInForce, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const RETURN_DAYS: usize = 7;
const ENTRY_THRESHOLD: f64 = 0.03;
const EXIT_THRESHOLD: f64 = -0.02;
const SIZING_MARGIN_PPM: i64 = 5_000;
const MIN_ROUND_TRIPS: usize = 30;

struct PendingOrder {
    buy: bool,
    remaining: Qty,
}

struct OutlierResistantDrift {
    current_day: Option<i64>,
    current_close: Price,
    current_day_finalized: bool,
    previous_day: Option<i64>,
    previous_close: Option<Price>,
    returns: VecDeque<f64>,
    pending: Option<PendingOrder>,
    round_trip_open: bool,
    completed_round_trips: usize,
}

impl OutlierResistantDrift {
    fn new() -> Self {
        Self {
            current_day: None,
            current_close: Price::from_raw(0),
            current_day_finalized: false,
            previous_day: None,
            previous_close: None,
            returns: VecDeque::with_capacity(RETURN_DAYS + 1),
            pending: None,
            round_trip_open: false,
            completed_round_trips: 0,
        }
    }

    fn complete_day(&mut self, day: i64, close: Price) -> Option<f64> {
        if let (Some(previous_day), Some(previous_close)) = (self.previous_day, self.previous_close) {
            if previous_day.checked_add(1) == Some(day) {
                let previous = previous_close.to_f64();
                let current = close.to_f64();
                if previous > 0.0 && current > 0.0 {
                    self.returns.push_back((current / previous).ln());
                    if self.returns.len() > RETURN_DAYS {
                        self.returns.pop_front();
                    }
                } else {
                    self.returns.clear();
                }
            } else {
                // Do not treat a multi-day gap as a single daily return.
                self.returns.clear();
            }
        }

        self.previous_day = Some(day);
        self.previous_close = Some(close);

        if self.returns.len() != RETURN_DAYS {
            return None;
        }

        let mut sum = 0.0;
        let mut largest_positive = 0.0;
        for daily_return in &self.returns {
            sum += *daily_return;
            if *daily_return > largest_positive {
                largest_positive = *daily_return;
            }
        }
        Some(sum - largest_positive)
    }

    fn submit_pending(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if ctx.open_orders() != 0 {
            return;
        }

        let Some((buy, remaining)) = self
            .pending
            .as_ref()
            .map(|pending| (pending.buy, pending.remaining))
        else {
            return;
        };

        if !remaining.is_positive() {
            self.pending = None;
            return;
        }

        // Orders are submitted after a bar closes. Use that completed bar's
        // volume as a tranche limit; any unfilled target is retried next bar.
        let volume_limit = bar.volume.mul_ratio(1, 10);
        let mut quantity = min_qty(remaining, volume_limit);

        if buy {
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            quantity = min_qty(quantity, affordable);
        } else {
            quantity = min_qty(quantity, ctx.position(INSTRUMENT));
        }

        if !quantity.is_positive() {
            return;
        }

        let side = if buy { Side::Buy } else { Side::Sell };
        let request = OrderRequest::market(INSTRUMENT, side, quantity)
            .with_time_in_force(TimeInForce::Ioc);
        let _ = ctx.submit(request);
    }

    fn act_on_signal(&mut self, ctx: &mut Ctx, bar: &Bar, robust_drift: f64) {
        if self.pending.is_some() || ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_zero() && robust_drift >= ENTRY_THRESHOLD {
            let quantity = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if quantity.is_positive() {
                self.pending = Some(PendingOrder {
                    buy: true,
                    remaining: quantity,
                });
                self.submit_pending(ctx, bar);
            }
        } else if position.is_positive() && robust_drift <= EXIT_THRESHOLD {
            self.pending = Some(PendingOrder {
                buy: false,
                remaining: position,
            });
            self.submit_pending(ctx, bar);
        }
    }
}

fn min_qty(a: Qty, b: Qty) -> Qty {
    if a < b { a } else { b }
}

impl Strategy for OutlierResistantDrift {
    fn name(&self) -> String {
        "outlier-resistant-drift".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        // A previous IOC may have filled only partially. Retry its remaining
        // target once the previous order has finished.
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| !pending.remaining.is_positive())
        {
            self.pending = None;
        }
        if self.pending.is_some() {
            if !self.pending.as_ref().is_some_and(|pending| !pending.buy)
                || ctx.position(INSTRUMENT).is_positive()
            {
                self.submit_pending(ctx, bar);
            } else {
                self.pending = None;
            }
        }

        let day_ns = TimeDelta::from_days(1).as_nanos();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let timestamp_ns = bar.close_ts.as_nanos();
        let day = timestamp_ns.div_euclid(day_ns);
        let minute_of_day = timestamp_ns.div_euclid(minute_ns).rem_euclid(1440);
        let mut signal = None;

        match self.current_day {
            None => {
                self.current_day = Some(day);
                self.current_close = bar.close;
                self.current_day_finalized = false;
            }
            Some(current_day) if current_day != day => {
                if !self.current_day_finalized {
                    signal = self.complete_day(current_day, self.current_close);
                }
                self.current_day = Some(day);
                self.current_close = bar.close;
                self.current_day_finalized = false;
            }
            Some(_) => {
                self.current_close = bar.close;
            }
        }

        // With complete minute data, this is the final bar of the UTC day.
        if minute_of_day == 1439 && !self.current_day_finalized {
            if let Some(current_day) = self.current_day {
                if let Some(robust_drift) = self.complete_day(current_day, self.current_close) {
                    signal = Some(robust_drift);
                }
                self.current_day_finalized = true;
            }
        }

        if let Some(robust_drift) = signal {
            self.act_on_signal(ctx, bar, robust_drift);
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &Fill) {
        let Some(pending) = self.pending.as_mut() else {
            return;
        };

        let matching_side = (pending.buy && fill.side == Side::Buy)
            || (!pending.buy && fill.side == Side::Sell);
        if !matching_side {
            return;
        }

        let remaining_raw = (pending.remaining.raw() - fill.qty.raw()).max(0);
        pending.remaining = Qty::from_raw(remaining_raw);

        if pending.buy {
            self.round_trip_open = true;
        } else if ctx.position(INSTRUMENT).is_zero() && self.round_trip_open {
            self.completed_round_trips += 1;
            self.round_trip_open = false;
        }
    }

    fn on_finish(&mut self, ctx: &mut Ctx) {
        if self.completed_round_trips < MIN_ROUND_TRIPS {
            let message = format!(
                "inconclusive: {} completed round trips (fewer than {})",
                self.completed_round_trips, MIN_ROUND_TRIPS
            );
            ctx.log(&message);
        }
    }
}

export_strategy!(OutlierResistantDrift::new());
