use std::collections::{BTreeMap, VecDeque};

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const WINDOW: usize = 2_880;
const SIZING_MARGIN_PPM: i64 = 5_000;
const PRICE_SCALE: i128 = 100_000_000;

#[derive(Clone, Copy)]
struct Sample {
    index: u64,
    low: i64,
    high: i64,
    volume: i64,
}

fn map_add(map: &mut BTreeMap<i64, usize>, value: i64) {
    *map.entry(value).or_insert(0) += 1;
}

fn map_remove(map: &mut BTreeMap<i64, usize>, value: i64) {
    if let Some(count) = map.get_mut(&value) {
        *count -= 1;
        if *count == 0 {
            map.remove(&value);
        }
    }
}

struct History {
    bars: VecDeque<Sample>,
    lows: VecDeque<(u64, i64)>,
    highs: VecDeque<(u64, i64)>,
    lower: BTreeMap<i64, usize>,
    upper: BTreeMap<i64, usize>,
    lower_len: usize,
    upper_len: usize,
    next_index: u64,
}

impl History {
    fn new() -> Self {
        Self {
            bars: VecDeque::with_capacity(WINDOW + 1),
            lows: VecDeque::with_capacity(WINDOW + 1),
            highs: VecDeque::with_capacity(WINDOW + 1),
            lower: BTreeMap::new(),
            upper: BTreeMap::new(),
            lower_len: 0,
            upper_len: 0,
            next_index: 0,
        }
    }

    fn move_lower_max_to_upper(&mut self) {
        if let Some((&value, _)) = self.lower.last_key_value() {
            map_remove(&mut self.lower, value);
            map_add(&mut self.upper, value);
            self.lower_len -= 1;
            self.upper_len += 1;
        }
    }

    fn move_upper_min_to_lower(&mut self) {
        if let Some((&value, _)) = self.upper.first_key_value() {
            map_remove(&mut self.upper, value);
            map_add(&mut self.lower, value);
            self.upper_len -= 1;
            self.lower_len += 1;
        }
    }

    fn rebalance(&mut self) {
        while self.lower_len > self.upper_len + 1 {
            self.move_lower_max_to_upper();
        }
        while self.lower_len < self.upper_len {
            self.move_upper_min_to_lower();
        }
    }

    fn add_volume(&mut self, value: i64) {
        let belongs_lower = self
            .lower
            .last_key_value()
            .is_none_or(|(&largest_lower, _)| value <= largest_lower);
        if belongs_lower {
            map_add(&mut self.lower, value);
            self.lower_len += 1;
        } else {
            map_add(&mut self.upper, value);
            self.upper_len += 1;
        }
        self.rebalance();
    }

    fn remove_volume(&mut self, value: i64) {
        if self.lower.contains_key(&value) {
            map_remove(&mut self.lower, value);
            self.lower_len -= 1;
        } else {
            map_remove(&mut self.upper, value);
            self.upper_len -= 1;
        }
        self.rebalance();
    }

    // Returns twice the median, preserving half-raw-unit medians exactly.
    fn median_twice(&self) -> Option<i128> {
        if self.bars.len() != WINDOW {
            return None;
        }
        let (&lower_middle, _) = self.lower.last_key_value()?;
        if self.lower_len == self.upper_len {
            let (&upper_middle, _) = self.upper.first_key_value()?;
            Some(i128::from(lower_middle) + i128::from(upper_middle))
        } else {
            Some(i128::from(lower_middle) * 2)
        }
    }

    fn range(&self) -> Option<(i64, i64)> {
        if self.bars.len() != WINDOW {
            return None;
        }
        Some((self.lows.front()?.1, self.highs.front()?.1))
    }

    fn push(&mut self, low: i64, high: i64, volume: i64) {
        if self.bars.len() == WINDOW {
            if let Some(old) = self.bars.pop_front() {
                self.remove_volume(old.volume);
                if self.lows.front().is_some_and(|(index, _)| *index == old.index) {
                    self.lows.pop_front();
                }
                if self.highs.front().is_some_and(|(index, _)| *index == old.index) {
                    self.highs.pop_front();
                }
            }
        }

        let index = self.next_index;
        self.next_index = self.next_index.wrapping_add(1);

        while self.lows.back().is_some_and(|(_, value)| *value >= low) {
            self.lows.pop_back();
        }
        self.lows.push_back((index, low));

        while self.highs.back().is_some_and(|(_, value)| *value <= high) {
            self.highs.pop_back();
        }
        self.highs.push_back((index, high));

        self.add_volume(volume);
        self.bars.push_back(Sample { index, low, high, volume });
    }
}

#[derive(Clone, Copy)]
struct EntryPlan {
    stop: Price,
    target: Price,
}

struct FailedLowReclaim {
    history: History,
    next_entry: Option<EntryPlan>,
    planned_entry: Option<EntryPlan>,
    active_entry: Option<EntryPlan>,
    entry_pending: bool,
    force_exit: bool,
    held_bars: usize,
    completed_round_trips: usize,
}

impl FailedLowReclaim {
    fn new() -> Self {
        Self {
            history: History::new(),
            next_entry: None,
            planned_entry: None,
            active_entry: None,
            entry_pending: false,
            force_exit: false,
            held_bars: 0,
            completed_round_trips: 0,
        }
    }

    fn submit_pending_entry(&mut self, ctx: &mut Ctx, bar: &Bar) {
        let Some(plan) = self.next_entry.take() else {
            return;
        };
        if ctx.open_orders() != 0 || !ctx.position(INSTRUMENT).is_zero() {
            return;
        }

        let entry = bar.open;
        if entry <= plan.stop
            || i128::from(plan.target.raw()) * 100 < i128::from(entry.raw()) * 101
            || entry.raw() <= plan.stop.raw()
        {
            return;
        }

        let stop_distance = i128::from(entry.raw()) - i128::from(plan.stop.raw());
        if stop_distance <= 0 {
            return;
        }
        let equity_raw = ctx.equity().raw().max(0);
        let risk_money_raw = i128::from(equity_raw) / 200;
        let risk_qty_raw = risk_money_raw * PRICE_SCALE / stop_distance;
        let risk_qty = Qty::from_raw(risk_qty_raw.clamp(0, i128::from(i64::MAX)) as i64);

        let cash_budget = ctx.cash().mul_ratio(95, 100);
        let cash_qty = ctx.affordable_qty(INSTRUMENT, entry, cash_budget, SIZING_MARGIN_PPM);
        let volume_cap = bar.volume.mul_ratio(1, 10);
        let qty = Qty::from_raw(
            risk_qty
                .raw()
                .min(cash_qty.raw())
                .min(volume_cap.raw())
                .max(0),
        );
        if !qty.is_positive() {
            return;
        }

        self.planned_entry = Some(plan);
        self.entry_pending = true;
        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
    }

    fn consider_signal(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if self.history.bars.len() != WINDOW
            || ctx.open_orders() != 0
            || !ctx.position(INSTRUMENT).is_zero()
            || self.entry_pending
            || self.next_entry.is_some()
        {
            return;
        }
        let (Some((low, high)), Some(median_twice)) =
            (self.history.range(), self.history.median_twice())
        else {
            return;
        };
        if low <= 0 {
            return;
        }

        let tight_range = i128::from(high) * 100 <= i128::from(low) * 112;
        let broke_low = i128::from(bar.low.raw()) * 1_000 <= i128::from(low) * 997;
        let reclaimed = i128::from(bar.close.raw()) * 1_000 >= i128::from(low) * 1_001;
        let elevated_volume = i128::from(bar.volume.raw()) * 4 >= median_twice * 3;
        if !(tight_range && broke_low && reclaimed && elevated_volume) {
            return;
        }

        let target_raw = ((i128::from(low) + i128::from(high)) / 2)
            .clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
        self.next_entry = Some(EntryPlan {
            stop: bar.low.mul_ratio(998, 1_000),
            target: Price::from_raw(target_raw),
        });
    }
}

impl Strategy for FailedLowReclaim {
    fn name(&self) -> String {
        "Failed-Low Reclaim".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        if self.entry_pending && ctx.position(INSTRUMENT).is_zero() && ctx.open_orders() == 0 {
            self.entry_pending = false;
            self.planned_entry = None;
        }

        self.submit_pending_entry(ctx, bar);

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            self.held_bars = self.held_bars.saturating_add(1);
            let hit_exit = self.force_exit
                || self.active_entry.is_some_and(|plan| {
                    bar.close <= plan.stop || bar.close >= plan.target
                })
                || self.held_bars >= 720;
            if hit_exit && ctx.open_orders() == 0 {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
        }

        self.consider_signal(ctx, bar);
        self.history
            .push(bar.low.raw(), bar.high.raw(), bar.volume.raw());
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.order_id == tape_guest::OrderId(0) {
            // The identifier is not used for strategy decisions.
        }
        match fill.side {
            Side::Buy => {
                self.entry_pending = false;
                self.active_entry = self.planned_entry;
                self.held_bars = 0;
                if let Some(plan) = self.active_entry {
                    self.force_exit = fill.price <= plan.stop
                        || i128::from(plan.target.raw()) * 100
                            < i128::from(fill.price.raw()) * 101;
                }
            }
            Side::Sell => {
                if ctx.position(INSTRUMENT).is_zero() {
                    self.completed_round_trips = self.completed_round_trips.saturating_add(1);
                    self.active_entry = None;
                    self.planned_entry = None;
                    self.force_exit = false;
                    self.held_bars = 0;
                }
            }
        }
    }

    fn on_finish(&mut self, ctx: &mut Ctx) {
        let message = format!(
            "Failed-Low Reclaim completed {} round trips; {} required for evaluation.",
            self.completed_round_trips, 30
        );
        ctx.log(&message);
    }
}

export_strategy!(FailedLowReclaim::new());
