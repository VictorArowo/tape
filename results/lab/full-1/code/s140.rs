use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIX_HOURS: usize = 6;
const LOOKBACK_RANGES: usize = 336;
const BREAKOUT_HOURS: usize = 24;
const SIZING_MARGIN_PPM: i64 = 5_000;
const RANGE_SCALE: f64 = 100_000_000.0;

struct HourBuild {
    hour_id: i64,
    high: Price,
    low: Price,
    close: Price,
    last_close_ts: tape_guest::Timestamp,
}

struct HourData {
    high: Price,
    low: Price,
    close: Price,
    range: Option<i64>,
    squeeze: bool,
}

struct CompressedRangeRelease {
    building: Option<HourBuild>,
    history: VecDeque<HourData>,
    pending_breakout: Option<Price>,
    active_breakout: Option<Price>,
    entry_ts: Option<tape_guest::Timestamp>,
}

impl CompressedRangeRelease {
    fn new() -> Self {
        Self {
            building: None,
            history: VecDeque::with_capacity(400),
            pending_breakout: None,
            active_breakout: None,
            entry_ts: None,
        }
    }

    fn complete_hour(&mut self, ctx: &mut Ctx, hour: HourBuild, close_ts: tape_guest::Timestamp) {
        self.history.push_back(HourData {
            high: hour.high,
            low: hour.low,
            close: hour.close,
            range: None,
            squeeze: false,
        });

        let range = if self.history.len() >= SIX_HOURS {
            let mut highest = i64::MIN;
            let mut lowest = i64::MAX;
            for completed in self.history.iter().rev().take(SIX_HOURS) {
                highest = highest.max(completed.high.raw());
                lowest = lowest.min(completed.low.raw());
            }
            let close = hour.close.to_f64();
            if close > 0.0 {
                Some((((highest - lowest) as f64 / close) * RANGE_SCALE).round() as i64)
            } else {
                None
            }
        } else {
            None
        };

        let squeeze = if let Some(current_range) = range {
            let mut preceding: Vec<i64> = self
                .history
                .iter()
                .rev()
                .skip(1)
                .filter_map(|completed| completed.range)
                .take(LOOKBACK_RANGES)
                .collect();
            if preceding.len() == LOOKBACK_RANGES {
                preceding.sort_unstable();
                let rank = (preceding.len() * 20 + 99) / 100;
                current_range < preceding[rank.saturating_sub(1)]
            } else {
                false
            }
        } else {
            false
        };

        if let Some(current) = self.history.back_mut() {
            current.range = range;
            current.squeeze = squeeze;
        }

        while self.history.len() > 400 {
            self.history.pop_front();
        }

        if ctx.open_orders() != 0 {
            return;
        }

        let previous_six: Vec<&HourData> = self
            .history
            .iter()
            .rev()
            .skip(1)
            .take(SIX_HOURS)
            .collect();
        let squeeze_present = previous_six.len() == SIX_HOURS
            && previous_six.iter().filter(|completed| completed.squeeze).count() >= 4;

        let previous_24: Vec<&HourData> = self
            .history
            .iter()
            .rev()
            .skip(1)
            .take(BREAKOUT_HOURS)
            .collect();

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            let hit_breakout = self
                .active_breakout
                .is_some_and(|level| hour.close <= level);
            let timed_exit = self.entry_ts.is_some_and(|entry| {
                close_ts.as_nanos().saturating_sub(entry.as_nanos())
                    >= TimeDelta::from_hours(72).as_nanos()
            });
            if (hit_breakout || timed_exit) && position.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
            return;
        }

        if !position.is_zero() || !squeeze_present || previous_24.len() != BREAKOUT_HOURS {
            return;
        }

        let Some(highest_previous) = previous_24.iter().map(|completed| completed.high).max()
        else {
            return;
        };
        if hour.close <= highest_previous {
            return;
        }

        let qty = ctx.affordable_qty(
            INSTRUMENT,
            hour.close,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        if qty.is_positive() {
            self.pending_breakout = Some(highest_previous);
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }
}

impl Strategy for CompressedRangeRelease {
    fn name(&self) -> String {
        "compressed-range-release".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let hour_ns = TimeDelta::from_hours(1).as_nanos();
        let open_hour = bar.open_ts.as_nanos() / hour_ns;
        let close_hour = bar.close_ts.as_nanos() / hour_ns;

        if self
            .building
            .as_ref()
            .is_some_and(|building| building.hour_id != open_hour)
        {
            if let Some(completed) = self.building.take() {
                let completed_at = completed.last_close_ts;
                self.complete_hour(ctx, completed, completed_at);
            }
        }

        if self.building.is_none() {
            self.building = Some(HourBuild {
                hour_id: open_hour,
                high: bar.high,
                low: bar.low,
                close: bar.close,
                last_close_ts: bar.close_ts,
            });
        } else if let Some(building) = self.building.as_mut() {
            building.high = building.high.max(bar.high);
            building.low = building.low.min(bar.low);
            building.close = bar.close;
            building.last_close_ts = bar.close_ts;
        }

        // The final minute of an hour closes at the hour boundary. Signals are
        // evaluated here, so submitted market orders act on a later bar.
        if close_hour > open_hour {
            if let Some(completed) = self.building.take() {
                self.complete_hour(ctx, completed, bar.close_ts);
            }
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        match fill.side {
            Side::Buy => {
                if self.entry_ts.is_none() {
                    self.active_breakout = self.pending_breakout.take().or(Some(fill.price));
                    self.entry_ts = Some(fill.ts);
                }
            }
            Side::Sell => {
                if ctx.position(INSTRUMENT).is_zero() {
                    self.active_breakout = None;
                    self.entry_ts = None;
                    self.pending_breakout = None;
                }
            }
        }
    }
}

export_strategy!(CompressedRangeRelease::new());