use std::collections::{BTreeMap, VecDeque};

use tape_guest::{
    Bar, Ctx, Fill, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, TimeInForce,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const RV_RETURN_WINDOW: usize = 6 * 60;
const BREAKOUT_WINDOW: usize = 6 * 60;
const EXIT_WINDOW: usize = 3 * 60;
const HISTORY_OBSERVATIONS: usize = 30 * 24 * 60;
const PARTICIPATION_NUM: i64 = 1;
const PARTICIPATION_DEN: i64 = 10;
const NO_LEVERAGE_MARGIN_PPM: i64 = 1_000_000;
const VOL_SCALE: f64 = 1.0e12;

struct VolObservation {
    ts: i64,
    value: i64,
}

// Maintains the smallest ceil(20% * n) observations in `lower`; its maximum
// is the nearest-rank 20th percentile. BTreeMaps keep rolling updates bounded.
struct RollingPercentile {
    lower: BTreeMap<i64, usize>,
    upper: BTreeMap<i64, usize>,
    lower_len: usize,
    len: usize,
}

impl RollingPercentile {
    fn new() -> Self {
        Self {
            lower: BTreeMap::new(),
            upper: BTreeMap::new(),
            lower_len: 0,
            len: 0,
        }
    }

    fn add_one(map: &mut BTreeMap<i64, usize>, value: i64) {
        let count = map.entry(value).or_insert(0);
        *count += 1;
    }

    fn remove_one(map: &mut BTreeMap<i64, usize>, value: i64) -> bool {
        let Some(count) = map.get(&value).copied() else {
            return false;
        };
        if count <= 1 {
            map.remove(&value);
        } else if let Some(count) = map.get_mut(&value) {
            *count -= 1;
        }
        true
    }

    fn target_lower_len(&self) -> usize {
        (self.len + 4) / 5
    }

    fn rebalance(&mut self) {
        let target = self.target_lower_len();
        while self.lower_len > target {
            let Some((&value, _)) = self.lower.last_key_value() else {
                break;
            };
            Self::remove_one(&mut self.lower, value);
            Self::add_one(&mut self.upper, value);
            self.lower_len -= 1;
        }
        while self.lower_len < target {
            let Some((&value, _)) = self.upper.first_key_value() else {
                break;
            };
            Self::remove_one(&mut self.upper, value);
            Self::add_one(&mut self.lower, value);
            self.lower_len += 1;
        }
    }

    fn insert(&mut self, value: i64) {
        let belongs_lower = self
            .lower
            .last_key_value()
            .map_or(true, |(&largest_lower, _)| value <= largest_lower);
        if belongs_lower {
            Self::add_one(&mut self.lower, value);
            self.lower_len += 1;
        } else {
            Self::add_one(&mut self.upper, value);
        }
        self.len += 1;
        self.rebalance();
    }

    fn remove(&mut self, value: i64) {
        if Self::remove_one(&mut self.lower, value) {
            self.lower_len -= 1;
        } else {
            Self::remove_one(&mut self.upper, value);
        }
        self.len = self.len.saturating_sub(1);
        self.rebalance();
    }

    fn percentile_20(&self) -> Option<i64> {
        self.lower.last_key_value().map(|(&value, _)| value)
    }
}

struct CompressionReleaseBreakout {
    previous_close: Option<f64>,
    returns: VecDeque<f64>,
    return_sum: f64,
    return_sum_squares: f64,
    prior_highs: VecDeque<i64>,
    prior_lows: VecDeque<i64>,
    volatility_history: VecDeque<VolObservation>,
    volatility_percentile: RollingPercentile,
    pending_entry: bool,
    exit_active: bool,
    entry_ts: Option<i64>,
}

impl CompressionReleaseBreakout {
    fn new() -> Self {
        Self {
            previous_close: None,
            returns: VecDeque::with_capacity(RV_RETURN_WINDOW + 1),
            return_sum: 0.0,
            return_sum_squares: 0.0,
            prior_highs: VecDeque::with_capacity(BREAKOUT_WINDOW + 1),
            prior_lows: VecDeque::with_capacity(EXIT_WINDOW + 1),
            volatility_history: VecDeque::with_capacity(HISTORY_OBSERVATIONS + 1),
            volatility_percentile: RollingPercentile::new(),
            pending_entry: false,
            exit_active: false,
            entry_ts: None,
        }
    }

    fn prune_volatility_history(&mut self, now: i64) {
        let window = TimeDelta::from_days(30).as_nanos();
        while self
            .volatility_history
            .front()
            .is_some_and(|observation| now.saturating_sub(observation.ts) > window)
        {
            if let Some(observation) = self.volatility_history.pop_front() {
                self.volatility_percentile.remove(observation.value);
            }
        }
    }

    fn add_return(&mut self, value: f64) {
        self.returns.push_back(value);
        self.return_sum += value;
        self.return_sum_squares += value * value;
        if self.returns.len() > RV_RETURN_WINDOW {
            if let Some(oldest) = self.returns.pop_front() {
                self.return_sum -= oldest;
                self.return_sum_squares -= oldest * oldest;
            }
        }
    }

    fn clear_returns(&mut self) {
        self.returns.clear();
        self.return_sum = 0.0;
        self.return_sum_squares = 0.0;
    }

    fn current_rv_key(&self) -> Option<i64> {
        if self.returns.len() != RV_RETURN_WINDOW {
            return None;
        }
        let count = RV_RETURN_WINDOW as f64;
        let mean = self.return_sum / count;
        let variance = (self.return_sum_squares / count - mean * mean).max(0.0);
        let volatility = variance.sqrt();
        volatility.is_finite().then_some((volatility * VOL_SCALE).round() as i64)
    }

    fn record_volatility(&mut self, ts: i64, value: i64) {
        if self.volatility_history.len() >= HISTORY_OBSERVATIONS {
            if let Some(oldest) = self.volatility_history.pop_front() {
                self.volatility_percentile.remove(oldest.value);
            }
        }
        self.volatility_history
            .push_back(VolObservation { ts, value });
        self.volatility_percentile.insert(value);
    }

    fn push_price_extremes(&mut self, bar: &Bar) {
        self.prior_highs.push_back(bar.high.raw());
        if self.prior_highs.len() > BREAKOUT_WINDOW {
            self.prior_highs.pop_front();
        }
        self.prior_lows.push_back(bar.low.raw());
        if self.prior_lows.len() > EXIT_WINDOW {
            self.prior_lows.pop_front();
        }
    }

    fn capped_qty(volume: Qty) -> Qty {
        volume.mul_ratio(PARTICIPATION_NUM, PARTICIPATION_DEN)
    }

    fn smaller_qty(a: Qty, b: Qty) -> Qty {
        Qty::from_raw(a.raw().min(b.raw()))
    }
}

impl Strategy for CompressionReleaseBreakout {
    fn name(&self) -> String {
        "compression-release-breakout [wasm]".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let now = bar.close_ts.as_nanos();
        self.prune_volatility_history(now);
        let mut submitted_this_bar = false;

        // Entry signals are queued from a completed bar and acted on at the
        // next bar callback. IOC ensures any unfilled quantity is cancelled.
        if self.pending_entry {
            if ctx.position(INSTRUMENT).is_positive() {
                self.pending_entry = false;
            } else if ctx.open_orders() == 0 {
                self.pending_entry = false;
                let cap = Self::capped_qty(bar.volume);
                if bar.open.raw() > 0 && cap.is_positive() {
                    let affordable = ctx.affordable_qty(
                        INSTRUMENT,
                        bar.open,
                        ctx.cash(),
                        NO_LEVERAGE_MARGIN_PPM,
                    );
                    let qty = Self::smaller_qty(affordable, cap);
                    if qty.is_positive() {
                        ctx.submit(
                            OrderRequest::market(INSTRUMENT, Side::Buy, qty)
                                .with_time_in_force(TimeInForce::Ioc),
                        );
                        submitted_this_bar = true;
                    }
                }
            }
        }

        let mut position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            if self.entry_ts.is_none() {
                self.entry_ts = Some(now);
            }

            // An exit condition latched on the prior completed bar is acted on
            // now; any remaining position is retried on subsequent bars.
            if self.exit_active && !submitted_this_bar && ctx.open_orders() == 0 {
                let cap = Self::capped_qty(bar.volume);
                let qty = Self::smaller_qty(position, cap);
                if bar.open.raw() > 0 && qty.is_positive() {
                    ctx.submit(
                        OrderRequest::market(INSTRUMENT, Side::Sell, qty)
                            .with_time_in_force(TimeInForce::Ioc),
                    );
                    submitted_this_bar = true;
                }
            }

            position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                let timed_out = self.entry_ts.is_some_and(|entry_ts| {
                    now.saturating_sub(entry_ts) >= TimeDelta::from_hours(24).as_nanos()
                });
                let broke_down = self.prior_lows.len() == EXIT_WINDOW
                    && self
                        .prior_lows
                        .iter()
                        .min()
                        .is_some_and(|lowest| bar.close.raw() < *lowest);
                if timed_out || broke_down {
                    self.exit_active = true;
                }
            } else {
                self.exit_active = false;
                self.entry_ts = None;
            }
        } else {
            self.pending_entry = false;
            self.exit_active = false;
            self.entry_ts = None;
        }

        // Update the trailing log-return window with this completed bar.
        let close_raw = bar.close.raw();
        if close_raw > 0 {
            let close = close_raw as f64;
            if let Some(previous) = self.previous_close {
                if previous > 0.0 {
                    let log_return = (close / previous).ln();
                    if log_return.is_finite() {
                        self.add_return(log_return);
                    } else {
                        self.clear_returns();
                    }
                } else {
                    self.clear_returns();
                }
            }
            self.previous_close = Some(close);
        } else {
            self.previous_close = None;
            self.clear_returns();
        }

        // The current RV is tested against prior observations before being
        // inserted, so it cannot influence its own 20th-percentile threshold.
        let current_rv = self.current_rv_key();
        let compressed = current_rv.is_some_and(|current| {
            self.volatility_history.len() >= HISTORY_OBSERVATIONS
                && self
                    .volatility_percentile
                    .percentile_20()
                    .is_some_and(|threshold| current <= threshold)
        });
        let breakout = self.prior_highs.len() == BREAKOUT_WINDOW
            && self
                .prior_highs
                .iter()
                .max()
                .is_some_and(|highest| bar.close.raw() > *highest);

        if let Some(value) = current_rv {
            self.record_volatility(now, value);
        }

        // Only queue a fresh entry while flat, with no outstanding order. The
        // queued order is consumed once; a later qualifying bar is a new signal.
        if !submitted_this_bar
            && !self.pending_entry
            && ctx.position(INSTRUMENT).is_zero()
            && ctx.open_orders() == 0
            && compressed
            && breakout
        {
            self.pending_entry = true;
        }

        self.push_price_extremes(bar);
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Buy {
            if self.entry_ts.is_none() {
                self.entry_ts = Some(fill.ts.as_nanos());
            }
            self.exit_active = false;
        } else if ctx.position(INSTRUMENT).is_zero() {
            self.entry_ts = None;
            self.exit_active = false;
        }
    }
}

export_strategy!(CompressionReleaseBreakout::new());
