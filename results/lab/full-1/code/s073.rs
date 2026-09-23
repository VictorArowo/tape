use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, Fill, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SLOTS_PER_DAY: i64 = 6;
const SLOTS_PER_WEEK: usize = 42;
const MINUTES_PER_BLOCK: i64 = 240;
const REQUIRED_OBSERVATIONS: usize = 26;
const SIZING_MARGIN_PPM: i64 = 5_000;
const ENTRY_THRESHOLD: f64 = 0.004;
const EXIT_THRESHOLD: f64 = 0.0005;
const PRICE_SCALE: f64 = 100_000_000.0;

struct AdaptiveWeeklySeasonality {
    closes: VecDeque<(i64, i64)>,
    outcomes: [VecDeque<f64>; SLOTS_PER_WEEK],
    tracked_qty_raw: i64,
    completed_round_trips: usize,
}

impl AdaptiveWeeklySeasonality {
    fn slot_at_boundary(ts: i64) -> Option<usize> {
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let block_ns = TimeDelta::from_hours(4).as_nanos();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();

        if day_ns <= 0 || block_ns <= 0 || minute_ns <= 0 || ts.rem_euclid(block_ns) != 0 {
            return None;
        }

        let day_of_week = ts.div_euclid(day_ns).rem_euclid(7);
        let minute_of_day = ts.rem_euclid(day_ns) / minute_ns;
        let slot_in_day = minute_of_day / MINUTES_PER_BLOCK;
        Some((day_of_week * SLOTS_PER_DAY + slot_in_day) as usize)
    }
}

impl Strategy for AdaptiveWeeklySeasonality {
    fn name(&self) -> String {
        "adaptive-weekly-seasonality".to_string()
    }

    fn on_start(&mut self, ctx: &mut Ctx) {
        self.tracked_qty_raw = ctx.position(INSTRUMENT).raw().max(0);
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let ts = bar.close_ts.as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        if day_ns <= 0 {
            return;
        }

        // A return becomes observable at its endpoint, exactly 24 hours after
        // the originating close. Keep only returns originating at 4-hour opens.
        let origin_ts = ts - day_ns;
        let origin_close = self
            .closes
            .iter()
            .find(|(close_ts, _)| *close_ts == origin_ts)
            .map(|(_, close_raw)| *close_raw);

        if let Some(origin_close_raw) = origin_close {
            if origin_close_raw > 0 && bar.close.is_positive() {
                if let Some(slot) = Self::slot_at_boundary(origin_ts) {
                    let origin_price = origin_close_raw as f64 / PRICE_SCALE;
                    let return_value = (bar.close.to_f64() / origin_price).ln();
                    if return_value.is_finite() {
                        let history = &mut self.outcomes[slot];
                        history.push_back(return_value);
                        while history.len() > REQUIRED_OBSERVATIONS {
                            history.pop_front();
                        }
                    }
                }
            }
        }

        while self
            .closes
            .front()
            .is_some_and(|(close_ts, _)| *close_ts < origin_ts)
        {
            self.closes.pop_front();
        }
        self.closes.push_back((ts, bar.close.raw()));

        // The preceding one-minute bar closes at the block boundary. An order
        // submitted here is handled by the sandbox's next-bar-open execution.
        let Some(slot) = Self::slot_at_boundary(ts) else {
            return;
        };
        let history = &self.outcomes[slot];
        if history.len() < REQUIRED_OBSERVATIONS || ctx.open_orders() != 0 {
            return;
        }

        let score = history.iter().sum::<f64>() / REQUIRED_OBSERVATIONS as f64;
        let position = ctx.position(INSTRUMENT);

        if position.is_zero() && score >= ENTRY_THRESHOLD {
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        } else if position.is_positive() && score < EXIT_THRESHOLD {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Buy {
            self.tracked_qty_raw = self.tracked_qty_raw.saturating_add(fill.qty.raw());
        } else {
            let previous_qty = self.tracked_qty_raw;
            self.tracked_qty_raw = self
                .tracked_qty_raw
                .saturating_sub(fill.qty.raw())
                .max(0);
            if previous_qty > 0 && self.tracked_qty_raw == 0 {
                self.completed_round_trips = self.completed_round_trips.saturating_add(1);
            }
        }
    }

    fn on_finish(&mut self, ctx: &mut Ctx) {
        let qualification = if self.completed_round_trips >= 30 {
            "meets the 30-round-trip minimum"
        } else {
            "does not meet the 30-round-trip minimum"
        };
        ctx.log(&format!(
            "Adaptive Weekly Seasonality: {} completed round trips; {} for meaningful testing.",
            self.completed_round_trips, qualification
        ));
    }
}

export_strategy!(AdaptiveWeeklySeasonality {
    closes: VecDeque::new(),
    outcomes: std::array::from_fn(|_| VecDeque::with_capacity(REQUIRED_OBSERVATIONS)),
    tracked_qty_raw: 0,
    completed_round_trips: 0,
});
