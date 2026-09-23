use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, Fill, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, Timestamp,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const VOL_WINDOW_HOURS: usize = 720;
const HISTORY_HOURS: usize = 32;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct HourBar {
    open: Price,
    high: Price,
    low: Price,
    close: Price,
}

struct HourAccumulator {
    key: i64,
    open: Price,
    high: Price,
    low: Price,
    close: Price,
}

struct CompressionRangeEscape {
    hours: VecDeque<HourBar>,
    volatilities: VecDeque<f64>,
    active_hour: Option<HourAccumulator>,
    entry_ts: Option<Timestamp>,
    trade_open: bool,
    round_trips: usize,
}

impl CompressionRangeEscape {
    fn highest_high(&self, count: usize, exclude_latest: usize) -> Option<i64> {
        let end = self.hours.len().checked_sub(exclude_latest)?;
        let start = end.checked_sub(count)?;
        self.hours
            .iter()
            .skip(start)
            .take(count)
            .map(|hour| hour.high.raw())
            .max()
    }

    fn lowest_low(&self, count: usize) -> Option<i64> {
        let start = self.hours.len().checked_sub(count)?;
        self.hours
            .iter()
            .skip(start)
            .take(count)
            .map(|hour| hour.low.raw())
            .min()
    }

    // Population standard deviation of the six close-to-close log returns.
    fn volatility_ending_now(&self) -> Option<f64> {
        if self.hours.len() < 7 {
            return None;
        }
        let closes: Vec<f64> = self
            .hours
            .iter()
            .skip(self.hours.len() - 7)
            .map(|hour| hour.close.to_f64())
            .collect();
        if closes.iter().any(|close| *close <= 0.0) {
            return None;
        }

        let returns: Vec<f64> = closes
            .windows(2)
            .map(|pair| (pair[1] / pair[0]).ln())
            .collect();
        let mean = returns.iter().sum::<f64>() / returns.len() as f64;
        let variance = returns
            .iter()
            .map(|value| {
                let difference = value - mean;
                difference * difference
            })
            .sum::<f64>()
            / returns.len() as f64;
        Some(variance.sqrt())
    }

    // The volatility deque contains the 720 completed-hour values ending at t-1.
    fn is_compressed(&self) -> bool {
        if self.volatilities.len() < VOL_WINDOW_HOURS {
            return false;
        }
        let Some(current) = self.volatilities.back().copied() else {
            return false;
        };
        let mut sorted: Vec<f64> = self.volatilities.iter().copied().collect();
        sorted.sort_by(|left, right| left.total_cmp(right));

        // Linearly interpolated empirical 20th percentile.
        let rank = 0.20 * (sorted.len() - 1) as f64;
        let lower = rank.floor() as usize;
        let upper = rank.ceil() as usize;
        let fraction = rank - lower as f64;
        let percentile = sorted[lower] + (sorted[upper] - sorted[lower]) * fraction;
        current <= percentile
    }

    fn on_hour_close(&mut self, ctx: &mut Ctx, hour: HourBar) {
        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            if ctx.open_orders() == 0
                && let Some(range_low) = self.lowest_low(6)
                && hour.close.raw() < range_low
            {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
        } else if position.is_zero() && ctx.open_orders() == 0 {
            let breakout = self
                .highest_high(12, 0)
                .zip(self.highest_high(12, 1))
                .zip(self.hours.back())
                .is_some_and(|((recent_high, prior_high), previous_hour)| {
                    hour.close.raw() > recent_high && previous_hour.close.raw() <= prior_high
                });

            if breakout && self.is_compressed() {
                let budget = ctx.cash().mul_ratio(95, 100);
                let qty = ctx.affordable_qty(
                    INSTRUMENT,
                    hour.close,
                    budget,
                    SIZING_MARGIN_PPM,
                );
                if qty.is_positive() {
                    // A market order submitted after this completed hour is intended
                    // to execute at the next one-minute bar open.
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                }
            }
        }

        self.hours.push_back(hour);
        while self.hours.len() > HISTORY_HOURS {
            self.hours.pop_front();
        }
        if let Some(volatility) = self.volatility_ending_now() {
            self.volatilities.push_back(volatility);
            while self.volatilities.len() > VOL_WINDOW_HOURS {
                self.volatilities.pop_front();
            }
        }
    }
}

impl Strategy for CompressionRangeEscape {
    fn name(&self) -> String {
        "compression-range-escape".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        // Time stop is checked each minute; the market order is submitted for the
        // next one-minute bar open, just like the hourly range exit.
        if ctx.position(INSTRUMENT).is_positive() && ctx.open_orders() == 0 {
            if let Some(entry_ts) = self.entry_ts {
                let elapsed = bar.close_ts.as_nanos().checked_sub(entry_ts.as_nanos());
                if elapsed.is_some_and(|value| value >= TimeDelta::from_hours(72).as_nanos()) {
                    ctx.submit(OrderRequest::market(
                        INSTRUMENT,
                        Side::Sell,
                        ctx.position(INSTRUMENT),
                    ));
                }
            }
        }

        let hour_ns = TimeDelta::from_hours(1).as_nanos();
        let key = bar.open_ts.as_nanos().div_euclid(hour_ns);
        if self.active_hour.as_ref().map(|hour| hour.key) != Some(key) {
            self.active_hour = Some(HourAccumulator {
                key,
                open: bar.open,
                high: bar.high,
                low: bar.low,
                close: bar.close,
            });
        } else if let Some(active) = self.active_hour.as_mut() {
            if bar.high > active.high {
                active.high = bar.high;
            }
            if bar.low < active.low {
                active.low = bar.low;
            }
            active.close = bar.close;
        }

        if bar.close_ts.as_nanos().rem_euclid(hour_ns) == 0
            && self.active_hour.as_ref().is_some_and(|hour| hour.key == key)
        {
            if let Some(active) = self.active_hour.take() {
                self.on_hour_close(
                    ctx,
                    HourBar {
                        open: active.open,
                        high: active.high,
                        low: active.low,
                        close: active.close,
                    },
                );
            }
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Buy {
            if self.entry_ts.is_none() {
                self.entry_ts = Some(fill.ts);
            }
            self.trade_open = true;
        } else if fill.side == Side::Sell && ctx.position(INSTRUMENT).is_zero() {
            if self.trade_open {
                self.round_trips += 1;
            }
            self.trade_open = false;
            self.entry_ts = None;
        }
    }

    fn on_finish(&mut self, ctx: &mut Ctx) {
        if self.round_trips < 30 {
            ctx.log(&format!(
                "Only {} completed round trips; reject this test as uninformative rather than tuning to it.",
                self.round_trips
            ));
        }
    }
}

export_strategy!(CompressionRangeEscape {
    hours: VecDeque::new(),
    volatilities: VecDeque::new(),
    active_hour: None,
    entry_ts: None,
    trade_open: false,
    round_trips: 0,
});
