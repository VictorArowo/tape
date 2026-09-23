use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeInForce, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const HOURS_6: usize = 360;
const HOURS_12: usize = 720;
const HOURS_24: usize = 1_440;
const RANGE_LOOKBACK: usize = 43_200;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct MonotonicDeque {
    entries: VecDeque<(usize, i64)>,
    maximum: bool,
}

impl MonotonicDeque {
    fn new(maximum: bool) -> Self {
        Self {
            entries: VecDeque::new(),
            maximum,
        }
    }

    fn push(&mut self, index: usize, value: i64) {
        while let Some((_, back)) = self.entries.back() {
            let dominated = if self.maximum {
                *back <= value
            } else {
                *back >= value
            };
            if !dominated {
                break;
            }
            self.entries.pop_back();
        }
        self.entries.push_back((index, value));
    }

    fn expire_before(&mut self, first_valid: usize) {
        while self
            .entries
            .front()
            .is_some_and(|(index, _)| *index < first_valid)
        {
            self.entries.pop_front();
        }
    }

    fn value(&self) -> Option<i64> {
        self.entries.front().map(|(_, value)| *value)
    }
}

struct Node {
    key: i64,
    priority: u64,
    count: usize,
    size: usize,
    left: Option<usize>,
    right: Option<usize>,
}

/// A deterministic treap supporting exact rolling order statistics.
struct OrderStats {
    root: Option<usize>,
    nodes: Vec<Node>,
    free: Vec<usize>,
    len: usize,
}

impl OrderStats {
    fn new() -> Self {
        Self {
            root: None,
            nodes: Vec::new(),
            free: Vec::new(),
            len: 0,
        }
    }

    fn mix(mut value: u64) -> u64 {
        value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn subtree_size(link: Option<usize>, nodes: &[Node]) -> usize {
        link.map_or(0, |index| nodes[index].size)
    }

    fn update(index: usize, nodes: &mut [Node]) {
        let left_size = Self::subtree_size(nodes[index].left, nodes);
        let right_size = Self::subtree_size(nodes[index].right, nodes);
        nodes[index].size = nodes[index].count + left_size + right_size;
    }

    fn rotate_right(root: usize, nodes: &mut [Node]) -> usize {
        let pivot = nodes[root].left.expect("right rotation requires a left child");
        let middle = nodes[pivot].right;
        nodes[root].left = middle;
        nodes[pivot].right = Some(root);
        Self::update(root, nodes);
        Self::update(pivot, nodes);
        pivot
    }

    fn rotate_left(root: usize, nodes: &mut [Node]) -> usize {
        let pivot = nodes[root].right.expect("left rotation requires a right child");
        let middle = nodes[pivot].left;
        nodes[root].right = middle;
        nodes[pivot].left = Some(root);
        Self::update(root, nodes);
        Self::update(pivot, nodes);
        pivot
    }

    fn insert_at(
        root: Option<usize>,
        key: i64,
        nodes: &mut Vec<Node>,
        free: &mut Vec<usize>,
    ) -> Option<usize> {
        let Some(index) = root else {
            let node = Node {
                key,
                priority: Self::mix(key as u64),
                count: 1,
                size: 1,
                left: None,
                right: None,
            };
            if let Some(reused) = free.pop() {
                nodes[reused] = node;
                return Some(reused);
            }
            nodes.push(node);
            return Some(nodes.len() - 1);
        };

        if key == nodes[index].key {
            nodes[index].count += 1;
            Self::update(index, nodes);
            return Some(index);
        }

        if key < nodes[index].key {
            let child = Self::insert_at(nodes[index].left, key, nodes, free);
            nodes[index].left = child;
            if let Some(child_index) = child {
                if nodes[child_index].priority > nodes[index].priority {
                    return Some(Self::rotate_right(index, nodes));
                }
            }
        } else {
            let child = Self::insert_at(nodes[index].right, key, nodes, free);
            nodes[index].right = child;
            if let Some(child_index) = child {
                if nodes[child_index].priority > nodes[index].priority {
                    return Some(Self::rotate_left(index, nodes));
                }
            }
        }

        Self::update(index, nodes);
        Some(index)
    }

    fn merge(left: Option<usize>, right: Option<usize>, nodes: &mut [Node]) -> Option<usize> {
        match (left, right) {
            (None, other) | (other, None) => other,
            (Some(left_index), Some(right_index)) => {
                if nodes[left_index].priority > nodes[right_index].priority {
                    let merged = Self::merge(nodes[left_index].right, Some(right_index), nodes);
                    nodes[left_index].right = merged;
                    Self::update(left_index, nodes);
                    Some(left_index)
                } else {
                    let merged = Self::merge(Some(left_index), nodes[right_index].left, nodes);
                    nodes[right_index].left = merged;
                    Self::update(right_index, nodes);
                    Some(right_index)
                }
            }
        }
    }

    fn remove_at(
        root: Option<usize>,
        key: i64,
        nodes: &mut Vec<Node>,
        free: &mut Vec<usize>,
    ) -> Option<usize> {
        let index = root?;
        if key == nodes[index].key {
            if nodes[index].count > 1 {
                nodes[index].count -= 1;
                Self::update(index, nodes);
                return Some(index);
            }
            let merged = Self::merge(nodes[index].left, nodes[index].right, nodes);
            free.push(index);
            return merged;
        }

        if key < nodes[index].key {
            let child = Self::remove_at(nodes[index].left, key, nodes, free);
            nodes[index].left = child;
        } else {
            let child = Self::remove_at(nodes[index].right, key, nodes, free);
            nodes[index].right = child;
        }
        Self::update(index, nodes);
        Some(index)
    }

    fn insert(&mut self, key: i64) {
        self.root = Self::insert_at(self.root, key, &mut self.nodes, &mut self.free);
        self.len += 1;
    }

    fn remove(&mut self, key: i64) {
        self.root = Self::remove_at(self.root, key, &mut self.nodes, &mut self.free);
        self.len = self.len.saturating_sub(1);
    }

    fn len(&self) -> usize {
        self.len
    }

    /// Returns the zero-based kth value in sorted order.
    fn select(&self, mut rank: usize) -> Option<i64> {
        let mut cursor = self.root;
        while let Some(index) = cursor {
            let left_size = Self::subtree_size(self.nodes[index].left, &self.nodes);
            if rank < left_size {
                cursor = self.nodes[index].left;
            } else if rank < left_size + self.nodes[index].count {
                return Some(self.nodes[index].key);
            } else {
                rank -= left_size + self.nodes[index].count;
                cursor = self.nodes[index].right;
            }
        }
        None
    }
}

struct CompressionRangeBreakout {
    bars_seen: usize,
    high_12h: MonotonicDeque,
    low_12h: MonotonicDeque,
    low_6h: MonotonicDeque,
    volumes: VecDeque<i64>,
    volume_sum: i128,
    range_samples: VecDeque<i64>,
    range_stats: OrderStats,
}

impl CompressionRangeBreakout {
    fn new() -> Self {
        Self {
            bars_seen: 0,
            high_12h: MonotonicDeque::new(true),
            low_12h: MonotonicDeque::new(false),
            low_6h: MonotonicDeque::new(false),
            volumes: VecDeque::with_capacity(HOURS_24 + 1),
            volume_sum: 0,
            range_samples: VecDeque::with_capacity(RANGE_LOOKBACK + 1),
            range_stats: OrderStats::new(),
        }
    }

    fn add_completed_bar(&mut self, bar: &Bar) {
        let index = self.bars_seen;
        self.high_12h.push(index, bar.high.raw());
        self.low_12h.push(index, bar.low.raw());
        self.low_6h.push(index, bar.low.raw());

        self.high_12h
            .expire_before((index + 1).saturating_sub(HOURS_12));
        self.low_12h
            .expire_before((index + 1).saturating_sub(HOURS_12));
        self.low_6h
            .expire_before((index + 1).saturating_sub(HOURS_6));

        self.volumes.push_back(bar.volume.raw());
        self.volume_sum += i128::from(bar.volume.raw());
        if self.volumes.len() > HOURS_24 {
            if let Some(old_volume) = self.volumes.pop_front() {
                self.volume_sum -= i128::from(old_volume);
            }
        }

        self.bars_seen += 1;

        // Record this completed bar's trailing 12-hour range only after using
        // the prior-bar distribution for the current signal.
        if self.bars_seen >= HOURS_12 {
            if let (Some(high), Some(low)) = (self.high_12h.value(), self.low_12h.value()) {
                let range = high.saturating_sub(low);
                if self.range_samples.len() == RANGE_LOOKBACK {
                    if let Some(old_range) = self.range_samples.pop_front() {
                        self.range_stats.remove(old_range);
                    }
                }
                self.range_samples.push_back(range);
                self.range_stats.insert(range);
            }
        }
    }
}

impl Strategy for CompressionRangeBreakout {
    fn name(&self) -> String {
        "compression-range-breakout".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let index = self.bars_seen;
        self.high_12h
            .expire_before(index.saturating_sub(HOURS_12));
        self.low_12h
            .expire_before(index.saturating_sub(HOURS_12));
        self.low_6h
            .expire_before(index.saturating_sub(HOURS_6));

        let prior_12h_ready = index >= HOURS_12;
        let prior_6h_ready = index >= HOURS_6;
        let prior_24h_volume_ready = self.volumes.len() == HOURS_24;

        let prior_range = if prior_12h_ready {
            match (self.high_12h.value(), self.low_12h.value()) {
                (Some(high), Some(low)) => Some(high.saturating_sub(low)),
                _ => None,
            }
        } else {
            None
        };

        if ctx.open_orders() == 0 {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                if prior_6h_ready
                    && self
                        .low_6h
                        .value()
                        .is_some_and(|low| bar.close.raw() < low)
                {
                    ctx.submit(
                        OrderRequest::market(INSTRUMENT, Side::Sell, position)
                            .with_time_in_force(TimeInForce::Ioc),
                    );
                }
            } else if position.is_zero()
                && prior_12h_ready
                && prior_24h_volume_ready
                && self.range_stats.len() == RANGE_LOOKBACK
            {
                let percentile_rank = (RANGE_LOOKBACK - 1) * 25 / 100;
                let percentile_25 = self.range_stats.select(percentile_rank);
                let compressed = match (prior_range, percentile_25) {
                    (Some(range), Some(percentile)) => range <= percentile,
                    _ => false,
                };
                let breakout = self
                    .high_12h
                    .value()
                    .is_some_and(|high| bar.close.raw() > high);
                let volume_surge =
                    i128::from(bar.volume.raw()) * (HOURS_24 as i128) * 4
                        > self.volume_sum * 5;

                if compressed && breakout && volume_surge {
                    let cash = ctx.cash();
                    let equity_budget = ctx.equity().mul_ratio(98, 100);
                    let budget = if equity_budget > cash {
                        cash
                    } else {
                        equity_budget
                    };
                    if budget.is_positive() {
                        let qty = ctx.affordable_qty(
                            INSTRUMENT,
                            bar.close,
                            budget,
                            SIZING_MARGIN_PPM,
                        );
                        if qty.is_positive() {
                            ctx.submit(
                                OrderRequest::market(INSTRUMENT, Side::Buy, qty)
                                    .with_time_in_force(TimeInForce::Ioc),
                            );
                        }
                    }
                }
            }
        }

        self.add_completed_bar(bar);
    }
}

export_strategy!(CompressionRangeBreakout::new());
