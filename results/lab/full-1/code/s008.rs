use std::collections::{BTreeMap, VecDeque};

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const WINDOW: usize = 10_080;
const VOLUME_WINDOW: usize = 30;
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Default)]
struct MultiSet {
    values: BTreeMap<i128, usize>,
    len: usize,
}

impl MultiSet {
    fn insert(&mut self, value: i128) {
        *self.values.entry(value).or_insert(0) += 1;
        self.len += 1;
    }

    fn remove(&mut self, value: i128) {
        if let Some(count) = self.values.get_mut(&value) {
            if *count == 1 {
                self.values.remove(&value);
            } else {
                *count -= 1;
            }
            self.len -= 1;
        }
    }

    fn pop_min(&mut self) -> Option<i128> {
        let value = *self.values.first_key_value()?.0;
        self.remove(value);
        Some(value)
    }

    fn pop_max(&mut self) -> Option<i128> {
        let value = *self.values.last_key_value()?.0;
        self.remove(value);
        Some(value)
    }

    fn min(&self) -> Option<i128> {
        self.values.first_key_value().map(|(&value, _)| value)
    }

    fn max(&self) -> Option<i128> {
        self.values.last_key_value().map(|(&value, _)| value)
    }
}

#[derive(Default)]
struct RollingMedian {
    lower: MultiSet,
    upper: MultiSet,
}

impl RollingMedian {
    fn len(&self) -> usize {
        self.lower.len + self.upper.len
    }

    fn insert(&mut self, value: i128) {
        if self.lower.max().is_none_or(|max| value <= max) {
            self.lower.insert(value);
        } else {
            self.upper.insert(value);
        }
        self.rebalance();
    }

    fn remove(&mut self, value: i128) {
        if self.lower.values.contains_key(&value) {
            self.lower.remove(value);
        } else {
            self.upper.remove(value);
        }
        self.rebalance();
    }

    fn rebalance(&mut self) {
        let target_lower = (self.len() + 1) / 2;
        while self.lower.len > target_lower {
            if let Some(value) = self.lower.pop_max() {
                self.upper.insert(value);
            }
        }
        while self.lower.len < target_lower {
            if let Some(value) = self.upper.pop_min() {
                self.lower.insert(value);
            } else {
                break;
            }
        }
    }

    // Returns the middle value(s); an even-sized median is their arithmetic mean.
    fn middle_values(&self) -> Option<(i128, i128)> {
        Some((self.lower.max()?, self.upper.min()?))
    }
}

struct LiquidationShockReversion {
    closes: VecDeque<i64>,
    sma_closes: VecDeque<i64>,
    sma_sum: i128,
    recent_volumes: VecDeque<i64>,
    recent_volume_sum: i128,
    historical_volume_sums: VecDeque<i128>,
    volume_median: RollingMedian,
    entry_price_raw: Option<i64>,
    entry_cost_raw: i128,
    entry_qty_raw: i128,
    entry_ts_ns: Option<i64>,
    held_bars: u16,
}

impl LiquidationShockReversion {
    fn new() -> Self {
        Self {
            closes: VecDeque::with_capacity(32),
            sma_closes: VecDeque::with_capacity(WINDOW + 1),
            sma_sum: 0,
            recent_volumes: VecDeque::with_capacity(VOLUME_WINDOW + 1),
            recent_volume_sum: 0,
            historical_volume_sums: VecDeque::with_capacity(WINDOW + 1),
            volume_median: RollingMedian::default(),
            entry_price_raw: None,
            entry_cost_raw: 0,
            entry_qty_raw: 0,
            entry_ts_ns: None,
            held_bars: 0,
        }
    }

    fn reset_trade(&mut self) {
        self.entry_price_raw = None;
        self.entry_cost_raw = 0;
        self.entry_qty_raw = 0;
        self.entry_ts_ns = None;
        self.held_bars = 0;
    }

    fn is_at_or_below_shock(close: i64, reference: i64) -> bool {
        i128::from(close) * 1_000 <= i128::from(reference) * 988
    }

    fn volume_is_elevated(current: i128, lower_middle: i128, upper_middle: i128) -> bool {
        // The baseline median is (lower_middle + upper_middle) / 2.
        4 * current >= 3 * (lower_middle + upper_middle)
    }

    fn update_history(&mut self, bar: &Bar) -> (Option<i128>, bool, bool) {
        let close = bar.close.raw();

        self.closes.push_back(close);
        if self.closes.len() > 32 {
            self.closes.pop_front();
        }

        self.sma_closes.push_back(close);
        self.sma_sum += i128::from(close);
        if self.sma_closes.len() > WINDOW {
            if let Some(old) = self.sma_closes.pop_front() {
                self.sma_sum -= i128::from(old);
            }
        }
        let trend_ok = self.sma_closes.len() == WINDOW
            && i128::from(close) * WINDOW as i128 > self.sma_sum;

        let volume = bar.volume.raw();
        self.recent_volumes.push_back(volume);
        self.recent_volume_sum += i128::from(volume);
        if self.recent_volumes.len() > VOLUME_WINDOW {
            if let Some(old) = self.recent_volumes.pop_front() {
                self.recent_volume_sum -= i128::from(old);
            }
        }
        let current_volume_sum = (self.recent_volumes.len() == VOLUME_WINDOW)
            .then_some(self.recent_volume_sum);

        let crossing = if self.closes.len() == 32 {
            let previous_reference = self.closes[0]; // C[t-31]
            let current_reference = self.closes[1]; // C[t-30]
            let previous_close = self.closes[30]; // C[t-1]
            previous_close > 0
                && previous_reference > 0
                && current_reference > 0
                && close > 0
                && i128::from(previous_close) * 1_000
                    > i128::from(previous_reference) * 988
                && Self::is_at_or_below_shock(close, current_reference)
        } else {
            false
        };

        (current_volume_sum, trend_ok, crossing)
    }

    fn record_current_volume_sum(&mut self, value: i128) {
        if self.historical_volume_sums.len() == WINDOW {
            if let Some(old) = self.historical_volume_sums.pop_front() {
                self.volume_median.remove(old);
            }
        }
        self.historical_volume_sums.push_back(value);
        self.volume_median.insert(value);
    }
}

impl Strategy for LiquidationShockReversion {
    fn name(&self) -> String {
        "liquidation-shock-reversion".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let (current_volume_sum, trend_ok, crossing) = self.update_history(bar);
        let volume_ok = match (current_volume_sum, self.volume_median.middle_values()) {
            (Some(current), Some((lower, upper))) if self.volume_median.len() == WINDOW => {
                Self::volume_is_elevated(current, lower, upper)
            }
            _ => false,
        };
        let entry_signal = crossing && volume_ok && trend_ok;

        // Add this bar's rolling sum only after evaluating it, so the baseline
        // consists of the preceding 10,080 rolling sums and excludes this bar.
        if let Some(current) = current_volume_sum {
            self.record_current_volume_sum(current);
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_zero() {
            self.reset_trade();
            if entry_signal && ctx.open_orders() == 0 {
                let budget = ctx.cash().mul_ratio(9, 10);
                let qty = ctx.affordable_qty(
                    INSTRUMENT,
                    bar.close,
                    budget,
                    SIZING_MARGIN_PPM,
                );
                if qty.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                }
            }
            return;
        }

        if !position.is_positive() {
            return;
        }

        if let Some(entry_ts_ns) = self.entry_ts_ns {
            if bar.close_ts.as_nanos() > entry_ts_ns {
                self.held_bars = self.held_bars.saturating_add(1);
            }
        }

        let Some(entry_price) = self.entry_price_raw else {
            return;
        };
        let close = i128::from(bar.close.raw());
        let entry = i128::from(entry_price);
        let take_profit = close * 100 >= entry * 101;
        let stop_loss = close * 1_000 <= entry * 985;
        let time_exit = self.held_bars >= 360;

        if (take_profit || stop_loss || time_exit) && ctx.open_orders() == 0 {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        match &fill.side {
            Side::Buy if fill.qty.is_positive() => {
                if self.entry_qty_raw == 0 {
                    self.entry_ts_ns = Some(fill.ts.as_nanos());
                    self.held_bars = 0;
                }
                let qty = i128::from(fill.qty.raw());
                self.entry_cost_raw += i128::from(fill.price.raw()) * qty;
                self.entry_qty_raw += qty;
                if self.entry_qty_raw > 0 {
                    self.entry_price_raw =
                        Some((self.entry_cost_raw / self.entry_qty_raw) as i64);
                }
            }
            Side::Sell => {
                if ctx.position(INSTRUMENT).is_zero() {
                    self.reset_trade();
                }
            }
            _ => {}
        }
    }
}

export_strategy!(LiquidationShockReversion::new());
