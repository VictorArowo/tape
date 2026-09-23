use std::collections::{BTreeMap, VecDeque};

use tape_guest::{
    Bar, Ctx, Fill, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, Timestamp,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const RANGE_BARS: usize = 360;
const BREAKOUT_BARS: usize = 720;
const VOLUME_BARS: usize = 60;
const DISTRIBUTION_SAMPLES: usize = 43_200;
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy)]
struct HistoryBar {
    high: i64,
    low: i64,
    volume: i64,
}

struct RangeDistribution {
    samples: VecDeque<i64>,
    lower: BTreeMap<i64, usize>,
    upper: BTreeMap<i64, usize>,
    lower_len: usize,
    upper_len: usize,
}

impl RangeDistribution {
    fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(DISTRIBUTION_SAMPLES + 1),
            lower: BTreeMap::new(),
            upper: BTreeMap::new(),
            lower_len: 0,
            upper_len: 0,
        }
    }

    fn increment(map: &mut BTreeMap<i64, usize>, value: i64) {
        *map.entry(value).or_insert(0) += 1;
    }

    fn decrement(map: &mut BTreeMap<i64, usize>, value: i64) {
        if let Some(count) = map.get_mut(&value) {
            *count -= 1;
            if *count == 0 {
                map.remove(&value);
            }
        }
    }

    fn rebalance(&mut self) {
        let total = self.lower_len + self.upper_len;
        let target = (total * 15 + 99) / 100;

        while self.lower_len > target {
            let value = *self.lower.last_key_value().expect("non-empty lower set").0;
            Self::decrement(&mut self.lower, value);
            Self::increment(&mut self.upper, value);
            self.lower_len -= 1;
            self.upper_len += 1;
        }
        while self.lower_len < target {
            let value = *self.upper.first_key_value().expect("non-empty upper set").0;
            Self::decrement(&mut self.upper, value);
            Self::increment(&mut self.lower, value);
            self.upper_len -= 1;
            self.lower_len += 1;
        }

        while let (Some((&low_max, _)), Some((&high_min, _))) =
            (self.lower.last_key_value(), self.upper.first_key_value())
        {
            if low_max <= high_min {
                break;
            }
            Self::decrement(&mut self.lower, low_max);
            Self::decrement(&mut self.upper, high_min);
            Self::increment(&mut self.lower, high_min);
            Self::increment(&mut self.upper, low_max);
        }
    }

    fn insert(&mut self, value: i64) {
        if self.lower.last_key_value().is_none_or(|(&largest, _)| value <= largest) {
            Self::increment(&mut self.lower, value);
            self.lower_len += 1;
        } else {
            Self::increment(&mut self.upper, value);
            self.upper_len += 1;
        }
        self.rebalance();
    }

    fn remove(&mut self, value: i64) {
        if self.lower.contains_key(&value) {
            Self::decrement(&mut self.lower, value);
            self.lower_len -= 1;
        } else {
            Self::decrement(&mut self.upper, value);
            self.upper_len -= 1;
        }
        self.rebalance();
    }

    fn push(&mut self, value: i64) {
        self.samples.push_back(value);
        self.insert(value);
        if self.samples.len() > DISTRIBUTION_SAMPLES {
            let oldest = self.samples.pop_front().expect("non-empty samples");
            self.remove(oldest);
        }
    }

    fn compression_threshold(&self) -> Option<i64> {
        (self.samples.len() == DISTRIBUTION_SAMPLES)
            .then(|| *self.lower.last_key_value().expect("non-empty lower set").0)
    }
}

struct CompressionReleaseBreakout {
    history: VecDeque<HistoryBar>,
    ranges: RangeDistribution,
    armed: bool,
    was_compressed: bool,
    entry_time: Option<Timestamp>,
    breakout_level: Option<i64>,
}

impl CompressionReleaseBreakout {
    fn prior_range(&self) -> Option<i64> {
        if self.history.len() < RANGE_BARS {
            return None;
        }
        let recent = self.history.iter().rev().take(RANGE_BARS);
        let high = recent.clone().map(|item| item.high).max()?;
        let low = recent.map(|item| item.low).min()?;
        Some(high.saturating_sub(low))
    }

    fn prior_breakout_high(&self) -> Option<i64> {
        if self.history.len() < BREAKOUT_BARS {
            return None;
        }
        self.history
            .iter()
            .rev()
            .take(BREAKOUT_BARS)
            .map(|item| item.high)
            .max()
    }

    fn volume_is_elevated(&self, current_volume: i64) -> bool {
        if self.history.len() < VOLUME_BARS {
            return false;
        }
        let mut volumes: Vec<i64> = self
            .history
            .iter()
            .rev()
            .take(VOLUME_BARS)
            .map(|item| item.volume)
            .collect();
        volumes.sort_unstable();
        let middle = VOLUME_BARS / 2;
        let median_twice = i128::from(volumes[middle - 1]) + i128::from(volumes[middle]);
        4 * i128::from(current_volume) > 3 * median_twice
    }
}

impl Strategy for CompressionReleaseBreakout {
    fn name(&self) -> String {
        "compression-release-breakout".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        // All lookbacks here use completed bars strictly before the current bar.
        let current_range = self.prior_range();
        let breakout_high = self.prior_breakout_high();
        let elevated_volume = self.volume_is_elevated(bar.volume.raw());
        let compressed = current_range
            .zip(self.ranges.compression_threshold())
            .is_some_and(|(range, threshold)| range <= threshold);

        if compressed && !self.was_compressed {
            self.armed = true;
        }
        self.was_compressed = compressed;

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            self.armed = false;
            let broke_level = self
                .breakout_level
                .is_some_and(|level| bar.close.raw() < level);
            let timed_out = self.entry_time.is_some_and(|entry| {
                let elapsed = i128::from(bar.close_ts.as_nanos())
                    - i128::from(entry.as_nanos());
                elapsed >= i128::from(TimeDelta::from_hours(72).as_nanos())
            });
            if ctx.open_orders() == 0 && (broke_level || timed_out) {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
        } else if self.armed && ctx.open_orders() == 0 && elevated_volume {
            if let Some(level) = breakout_high {
                if bar.close.raw() > level {
                    let qty = ctx.affordable_qty(
                        INSTRUMENT,
                        bar.close,
                        ctx.cash(),
                        SIZING_MARGIN_PPM,
                    );
                    if qty.is_positive() {
                        self.breakout_level = Some(level);
                        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                    }
                }
            }
        }

        // Add this bar's prior-six-hour range only after evaluating it, so the
        // percentile threshold contains observations strictly before this bar.
        if let Some(range) = current_range {
            self.ranges.push(range);
        }
        self.history.push_back(HistoryBar {
            high: bar.high.raw(),
            low: bar.low.raw(),
            volume: bar.volume.raw(),
        });
        if self.history.len() > BREAKOUT_BARS {
            self.history.pop_front();
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Buy {
            if self.entry_time.is_none() {
                self.entry_time = Some(fill.ts);
            }
            self.armed = false;
        } else if fill.side == Side::Sell && ctx.position(INSTRUMENT).is_zero() {
            self.entry_time = None;
            self.breakout_level = None;
            self.armed = false;
        }
    }
}

export_strategy!(CompressionReleaseBreakout {
    history: VecDeque::with_capacity(BREAKOUT_BARS + 1),
    ranges: RangeDistribution::new(),
    armed: false,
    was_compressed: false,
    entry_time: None,
    breakout_level: None,
});
