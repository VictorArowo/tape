use std::collections::{BTreeSet, VecDeque};

use tape_guest::{
    Bar, Ctx, InstrumentId, Money, OrderRequest, Price, Qty, Side, Strategy, TimeDelta,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SCALE: i128 = 100_000_000;
const LOOKBACK: usize = 12 * 60;
const ATR_LEN: usize = 60;
const WIDTH_HISTORY: usize = 14 * 24 * 60;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct WidthPercentile {
    values: VecDeque<(i64, usize)>,
    lower: BTreeSet<(i64, usize)>,
    upper: BTreeSet<(i64, usize)>,
}

impl WidthPercentile {
    fn new() -> Self {
        Self {
            values: VecDeque::with_capacity(WIDTH_HISTORY + 1),
            lower: BTreeSet::new(),
            upper: BTreeSet::new(),
        }
    }

    fn rebalance(&mut self) {
        // The lower set contains the nearest-rank 25th percentile and all
        // observations below it.
        let target = (self.values.len() + 3) / 4;
        while self.lower.len() > target {
            if let Some(item) = self.lower.iter().next_back().copied() {
                self.lower.remove(&item);
                self.upper.insert(item);
            } else {
                break;
            }
        }
        while self.lower.len() < target {
            if let Some(item) = self.upper.iter().next().copied() {
                self.upper.remove(&item);
                self.lower.insert(item);
            } else {
                break;
            }
        }
        loop {
            let (Some(low_max), Some(high_min)) = (
                self.lower.iter().next_back().copied(),
                self.upper.iter().next().copied(),
            ) else {
                break;
            };
            if low_max <= high_min {
                break;
            }
            self.lower.remove(&low_max);
            self.upper.remove(&high_min);
            self.lower.insert(high_min);
            self.upper.insert(low_max);
        }
    }

    fn insert(&mut self, item: (i64, usize)) {
        if self.lower.iter().next_back().map_or(true, |last| item <= *last) {
            self.lower.insert(item);
        } else {
            self.upper.insert(item);
        }
        self.rebalance();
    }

    fn remove(&mut self, item: (i64, usize)) {
        if !self.lower.remove(&item) {
            self.upper.remove(&item);
        }
        self.rebalance();
    }

    fn push(&mut self, value: i64, id: usize) {
        if self.values.len() == WIDTH_HISTORY {
            if let Some(old) = self.values.pop_front() {
                self.remove(old);
            }
        }
        let item = (value, id);
        self.values.push_back(item);
        self.insert(item);
    }

    fn threshold(&self) -> Option<i64> {
        if self.values.len() == WIDTH_HISTORY {
            self.lower.iter().next_back().map(|item| item.0)
        } else {
            None
        }
    }
}

struct CompressedRangeRelease {
    prior_bars: VecDeque<(i64, i64)>,
    true_ranges: VecDeque<i64>,
    true_range_sum: i128,
    widths: WidthPercentile,
    next_width_id: usize,
    last_close: Option<Price>,
    last_compressed: bool,

    entry_pending: bool,
    active_trade: bool,
    entry_ts_ns: Option<i64>,
    breakout_level: Option<Price>,
    stop_level: Option<Price>,
    exit_intent: bool,
}

impl CompressedRangeRelease {
    fn new() -> Self {
        Self {
            prior_bars: VecDeque::with_capacity(LOOKBACK),
            true_ranges: VecDeque::with_capacity(ATR_LEN),
            true_range_sum: 0,
            widths: WidthPercentile::new(),
            next_width_id: 0,
            last_close: None,
            last_compressed: false,
            entry_pending: false,
            active_trade: false,
            entry_ts_ns: None,
            breakout_level: None,
            stop_level: None,
            exit_intent: false,
        }
    }

    fn prior_range(&self) -> Option<(i64, i64)> {
        if self.prior_bars.len() != LOOKBACK {
            return None;
        }
        let mut high = i64::MIN;
        let mut low = i64::MAX;
        for &(bar_high, bar_low) in &self.prior_bars {
            high = high.max(bar_high);
            low = low.min(bar_low);
        }
        Some((high, low))
    }

    fn push_true_range(&mut self, bar: &Bar, previous_close: Option<Price>) {
        let high = i128::from(bar.high.raw());
        let low = i128::from(bar.low.raw());
        let mut tr = (high - low).max(0);
        if let Some(previous) = previous_close {
            let previous = i128::from(previous.raw());
            tr = tr.max((high - previous).abs());
            tr = tr.max((low - previous).abs());
        }
        let tr = tr.clamp(0, i128::from(i64::MAX)) as i64;
        if self.true_ranges.len() == ATR_LEN {
            if let Some(old) = self.true_ranges.pop_front() {
                self.true_range_sum -= i128::from(old);
            }
        }
        self.true_ranges.push_back(tr);
        self.true_range_sum += i128::from(tr);
    }

    fn atr_raw(&self) -> Option<i64> {
        (self.true_ranges.len() == ATR_LEN).then(|| {
            (self.true_range_sum / ATR_LEN as i128).clamp(0, i128::from(i64::MAX)) as i64
        })
    }

    fn entry_quantity(ctx: &mut Ctx, price: Price) -> Qty {
        let budget: Money = ctx.cash().mul_ratio(95, 100);
        if !budget.is_positive() || !price.is_positive() {
            return Qty::from_raw(0);
        }

        let affordable = ctx.affordable_qty(INSTRUMENT, price, budget, SIZING_MARGIN_PPM);
        // Enforce a cash-only notional limit even if the affordability helper
        // permits margin. The helper also accounts for purchase fees.
        let cash_cap_raw = (i128::from(budget.raw()) * SCALE / i128::from(price.raw()))
            .clamp(0, i128::from(i64::MAX)) as i64;
        let cash_cap = Qty::from_raw(cash_cap_raw);
        if affordable < cash_cap {
            affordable
        } else {
            cash_cap
        }
    }

    fn reset_trade(&mut self) {
        self.active_trade = false;
        self.entry_ts_ns = None;
        self.breakout_level = None;
        self.stop_level = None;
        self.exit_intent = false;
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

        let previous_close = self.last_close;
        let compressed_at_previous_bar = self.last_compressed;
        let prior_range = self.prior_range();

        self.push_true_range(bar, previous_close);
        let atr_raw = self.atr_raw();

        // This bar's width uses the preceding 12 hours and is compared with
        // the 14-day distribution accumulated before this bar.
        let current_width = prior_range.and_then(|(high, low)| {
            if bar.close.raw() <= 0 {
                return None;
            }
            let width_raw = ((i128::from(high) - i128::from(low)).max(0) * SCALE
                / i128::from(bar.close.raw()))
            .clamp(0, i128::from(i64::MAX)) as i64;
            Some(width_raw)
        });
        let compressed_now = current_width
            .zip(self.widths.threshold())
            .is_some_and(|(width, threshold)| width <= threshold);

        let breakout_high = prior_range.map(|(high, _)| Price::from_raw(high));
        let crossed_up = breakout_high.is_some_and(|high| {
            previous_close.is_some_and(|prior| prior <= high && bar.close > high)
        });
        let entry_signal = compressed_at_previous_bar && crossed_up;

        if let Some(width) = current_width {
            let id = self.next_width_id;
            self.next_width_id = self.next_width_id.wrapping_add(1);
            self.widths.push(width, id);
        }
        if self.prior_bars.len() == LOOKBACK {
            self.prior_bars.pop_front();
        }
        self.prior_bars.push_back((bar.high.raw(), bar.low.raw()));
        self.last_close = Some(bar.close);
        self.last_compressed = compressed_now;

        let position = ctx.position(INSTRUMENT);
        if position.is_zero() {
            if self.active_trade || self.exit_intent {
                self.reset_trade();
            }
            if self.entry_pending && ctx.open_orders() == 0 {
                self.entry_pending = false;
                self.breakout_level = None;
                self.stop_level = None;
            }
        } else if position.is_positive() {
            self.entry_pending = false;
        }

        if position.is_positive() {
            if self.active_trade {
                let below_breakout = self
                    .breakout_level
                    .is_some_and(|level| bar.close < level);
                let below_stop = self.stop_level.is_some_and(|level| bar.close < level);
                let timed_out = self.entry_ts_ns.is_some_and(|entry_ns| {
                    bar.close_ts.as_nanos().saturating_sub(entry_ns)
                        >= TimeDelta::from_hours(72).as_nanos()
                });
                if below_breakout || below_stop || timed_out {
                    self.exit_intent = true;
                }
            }

            if self.exit_intent && ctx.open_orders() == 0 {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
            return;
        }

        if !position.is_zero()
            || self.entry_pending
            || self.exit_intent
            || ctx.open_orders() != 0
        {
            return;
        }

        if entry_signal {
            let (Some(high), Some(atr)) = (breakout_high, atr_raw) else {
                return;
            };
            let qty = Self::entry_quantity(ctx, bar.close);
            if qty.is_positive() {
                let stop_raw = (i128::from(bar.close.raw()) - 2 * i128::from(atr))
                    .clamp(i128::from(i64::MIN), i128::from(i64::MAX))
                    as i64;
                self.breakout_level = Some(high);
                self.stop_level = Some(Price::from_raw(stop_raw));
                self.entry_pending = true;
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.side == Side::Buy && !self.active_trade {
            self.active_trade = true;
            self.entry_pending = false;
            self.entry_ts_ns = Some(fill.ts.as_nanos());
        }
    }
}

export_strategy!(CompressedRangeRelease::new());
