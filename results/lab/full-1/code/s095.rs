use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Intent {
    None,
    Buy,
    Sell,
}

struct SkipLagMomentum {
    history: VecDeque<(i64, i64)>,
    intent: Intent,
    remaining: Qty,
    accounted_position_raw: i64,
    completed_round_trips: u64,
}

impl SkipLagMomentum {
    fn close_at(&self, target_ts: i64) -> Option<i64> {
        self.history
            .iter()
            .rev()
            .find(|(ts, _)| *ts == target_ts)
            .map(|(_, close_raw)| *close_raw)
    }

    fn clear_intent(&mut self) {
        self.intent = Intent::None;
        self.remaining = Qty::from_raw(0);
    }

    fn min_qty(a: Qty, b: Qty) -> Qty {
        Qty::from_raw(a.raw().min(b.raw()).max(0))
    }
}

impl Strategy for SkipLagMomentum {
    fn name(&self) -> String {
        "skip-lag-momentum [wasm]".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let now = bar.close_ts.as_nanos();
        self.history.push_back((now, bar.close.raw()));
        let max_age = TimeDelta::from_hours(132).as_nanos();
        let cutoff = now.saturating_sub(max_age);
        while self.history.front().is_some_and(|(ts, _)| *ts < cutoff) {
            self.history.pop_front();
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let minute_of_day = (now / minute_ns) % 1440;
        if minute_of_day % (6 * 60) == 0 {
            let lag_12h = TimeDelta::from_hours(12).as_nanos();
            let lag_132h = TimeDelta::from_hours(132).as_nanos();
            let recent = self.close_at(now.saturating_sub(lag_12h));
            let old = self.close_at(now.saturating_sub(lag_132h));

            if let (Some(recent_raw), Some(old_raw)) = (recent, old) {
                if old_raw > 0 && recent_raw > 0 {
                    let momentum = recent_raw as f64 / old_raw as f64 - 1.0;
                    let position = ctx.position(INSTRUMENT);

                    if position.is_zero() && momentum > 0.01 {
                        let qty = ctx.affordable_qty(
                            INSTRUMENT,
                            bar.close,
                            ctx.cash(),
                            SIZING_MARGIN_PPM,
                        );
                        if qty.is_positive() {
                            self.intent = Intent::Buy;
                            self.remaining = qty;
                        } else {
                            self.clear_intent();
                        }
                    } else if position.is_positive() && momentum < 0.0 {
                        self.intent = Intent::Sell;
                        self.remaining = position;
                    } else {
                        self.clear_intent();
                    }
                }
            }
        }

        if ctx.open_orders() != 0 {
            return;
        }

        let volume_cap = bar.volume.mul_ratio(1, 10);
        if !volume_cap.is_positive() || !self.remaining.is_positive() {
            return;
        }

        match self.intent {
            Intent::Buy => {
                let affordable = ctx.affordable_qty(
                    INSTRUMENT,
                    bar.close,
                    ctx.cash(),
                    SIZING_MARGIN_PPM,
                );
                let qty = Self::min_qty(Self::min_qty(self.remaining, volume_cap), affordable);
                if qty.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                }
            }
            Intent::Sell => {
                let position = ctx.position(INSTRUMENT);
                let qty = Self::min_qty(Self::min_qty(self.remaining, volume_cap), position);
                if qty.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
                } else if position.is_zero() {
                    self.clear_intent();
                }
            }
            Intent::None => {}
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        let before = self.accounted_position_raw;
        match &fill.side {
            Side::Buy => {
                self.accounted_position_raw = self
                    .accounted_position_raw
                    .saturating_add(fill.qty.raw())
                    .max(0);
                if self.intent == Intent::Buy {
                    self.remaining = Qty::from_raw(
                        self.remaining.raw().saturating_sub(fill.qty.raw()).max(0),
                    );
                }
            }
            Side::Sell => {
                self.accounted_position_raw = self
                    .accounted_position_raw
                    .saturating_sub(fill.qty.raw())
                    .max(0);
                if self.intent == Intent::Sell {
                    self.remaining = Qty::from_raw(
                        self.remaining.raw().saturating_sub(fill.qty.raw()).max(0),
                    );
                }
            }
        }

        if before > 0 && self.accounted_position_raw == 0 {
            self.completed_round_trips = self.completed_round_trips.saturating_add(1);
        }
        if self.remaining.is_zero() {
            self.clear_intent();
        }
    }

    fn on_finish(&mut self, ctx: &mut Ctx) {
        ctx.log(&format!(
            "completed_round_trips={}",
            self.completed_round_trips
        ));
    }
}

export_strategy!(SkipLagMomentum {
    history: VecDeque::new(),
    intent: Intent::None,
    remaining: Qty::from_raw(0),
    accounted_position_raw: 0,
    completed_round_trips: 0,
});
