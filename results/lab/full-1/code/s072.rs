use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SLOT_COUNT: usize = 168;
const SIZING_MARGIN_PPM: i64 = 5_000;
const ENTRY_THRESHOLD: f64 = 0.003;

struct HourSample {
    ts: i64,
    price: i64,
    slot: usize,
}

struct ReturnObservation {
    completed_ts: i64,
    log_return: f64,
}

struct WeeklyClockDrift {
    hourly_samples: VecDeque<HourSample>,
    returns_by_slot: Vec<VecDeque<ReturnObservation>>,
    exit_due_ts: Option<i64>,
    was_long: bool,
    completed_round_trips: usize,
}

impl WeeklyClockDrift {
    fn new() -> Self {
        Self {
            hourly_samples: VecDeque::with_capacity(6),
            returns_by_slot: (0..SLOT_COUNT).map(|_| VecDeque::new()).collect(),
            exit_due_ts: None,
            was_long: false,
            completed_round_trips: 0,
        }
    }

    fn hour_of_week(ts: i64) -> usize {
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let minute_of_day = (ts / minute_ns).rem_euclid(1440);
        let day_of_week = (ts / day_ns).rem_euclid(7);
        (day_of_week * 24 + minute_of_day / 60) as usize
    }

    fn update_hourly_history(&mut self, ts: i64, price: i64) -> Option<f64> {
        let current_slot = Self::hour_of_week(ts);
        let four_hours_ns = TimeDelta::from_hours(4).as_nanos();
        let target_ts = ts - four_hours_ns;

        let prior = self
            .hourly_samples
            .iter()
            .find(|sample| sample.ts == target_ts)
            .map(|sample| (sample.price, sample.slot));

        if let Some((start_price, start_slot)) = prior {
            if start_price > 0 && price > 0 {
                let log_return = (price as f64 / start_price as f64).ln();
                self.returns_by_slot[start_slot].push_back(ReturnObservation {
                    completed_ts: ts,
                    log_return,
                });
            }
        }

        self.hourly_samples.push_back(HourSample {
            ts,
            price,
            slot: current_slot,
        });
        while self
            .hourly_samples
            .front()
            .is_some_and(|sample| sample.ts < target_ts)
        {
            self.hourly_samples.pop_front();
        }

        let cutoff = ts - TimeDelta::from_days(365).as_nanos();
        for observations in &mut self.returns_by_slot {
            while observations
                .front()
                .is_some_and(|observation| observation.completed_ts < cutoff)
            {
                observations.pop_front();
            }
        }

        let observations = &self.returns_by_slot[current_slot];
        if observations.len() < 30 {
            return None;
        }
        let mean = observations
            .iter()
            .map(|observation| observation.log_return)
            .sum::<f64>()
            / observations.len() as f64;
        Some(mean)
    }

    fn participation_cap(volume: Qty) -> Qty {
        volume.mul_ratio(1, 10)
    }
}

impl Strategy for WeeklyClockDrift {
    fn name(&self) -> String {
        "weekly-clock-drift".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let minute_index = bar.close_ts.as_nanos() / minute_ns;
        let minute_of_hour = minute_index.rem_euclid(60);
        let mean_return = if minute_of_hour == 0 {
            self.update_hourly_history(bar.close_ts.as_nanos(), bar.close.raw())
        } else {
            None
        };

        let position = ctx.position(INSTRUMENT);
        let mut just_closed = false;
        if position.is_positive() {
            self.was_long = true;
        } else if self.was_long {
            self.was_long = false;
            self.exit_due_ts = None;
            self.completed_round_trips += 1;
            just_closed = true;
        }

        if ctx.open_orders() != 0 || just_closed {
            return;
        }

        if position.is_positive() {
            if self
                .exit_due_ts
                .is_some_and(|due| bar.close_ts.as_nanos() >= due)
            {
                let cap = Self::participation_cap(bar.volume);
                let qty = if position < cap { position } else { cap };
                if qty.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
                }
            }
            return;
        }

        if let Some(mean) = mean_return {
            if mean > ENTRY_THRESHOLD {
                // This order is submitted at the hour boundary for execution at the
                // following one-minute bar's open.
                let affordable = ctx.affordable_qty(
                    INSTRUMENT,
                    bar.close,
                    ctx.cash(),
                    SIZING_MARGIN_PPM,
                );
                let cap = Self::participation_cap(bar.volume);
                let qty = if affordable < cap { affordable } else { cap };
                if qty.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                }
            }
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.side == Side::Buy && fill.qty.is_positive() {
            self.exit_due_ts = Some(fill.ts.as_nanos() + TimeDelta::from_hours(4).as_nanos());
        }
    }

    fn on_finish(&mut self, ctx: &mut Ctx) {
        ctx.log(&format!(
            "weekly-clock-drift completed round trips: {}",
            self.completed_round_trips
        ));
    }
}

export_strategy!(WeeklyClockDrift::new());
