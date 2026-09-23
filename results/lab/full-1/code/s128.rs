use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const HISTORY_LIMIT: usize = 30;
const MIN_HISTORY: usize = 20;
const MINUTE_NS: i64 = TimeDelta::from_mins(1).as_nanos();
const SIZING_MARGIN_PPM: i64 = 5_000;
const RAW_SCALE: i128 = 100_000_000;

#[derive(Clone, Copy)]
struct MinuteBar {
    ts_ns: i64,
    open_raw: i64,
    low_raw: i64,
    close_raw: i64,
    volume_raw: i64,
}

struct PostWindow {
    boundary_ns: i64,
    first_open_raw: Option<i64>,
    low_raw: i64,
}

#[derive(Clone, Copy)]
struct PositionState {
    qty_raw: i64,
    entry_price_raw: i64,
    entry_ts_ns: i64,
    post_low_raw: i64,
    exit_triggered: bool,
}

struct FundingWindowFade {
    bars: VecDeque<MinuteBar>,
    prior_volumes: [VecDeque<i128>; 3],
    post_window: Option<PostWindow>,
    position_state: Option<PositionState>,
    entry_low_for_fill: Option<i64>,
}

impl FundingWindowFade {
    fn new() -> Self {
        Self {
            bars: VecDeque::with_capacity(31),
            prior_volumes: std::array::from_fn(|_| VecDeque::with_capacity(HISTORY_LIMIT)),
            post_window: None,
            position_state: None,
            entry_low_for_fill: None,
        }
    }

    fn pre_boundary_stats(&self) -> Option<(i128, i64, i64)> {
        if self.bars.len() < 30 {
            return None;
        }
        let first_index = self.bars.len() - 30;
        let mut iter = self.bars.iter().skip(first_index);
        let first = *iter.next()?;
        let mut previous_ts = first.ts_ns;
        let mut last = first;
        let mut volume_sum = i128::from(first.volume_raw);

        for item in iter {
            if item.ts_ns - previous_ts != MINUTE_NS {
                return None;
            }
            previous_ts = item.ts_ns;
            last = *item;
            volume_sum += i128::from(item.volume_raw);
        }
        Some((volume_sum, first.open_raw, last.close_raw))
    }

    fn median_twice(values: &VecDeque<i128>) -> Option<i128> {
        if values.is_empty() {
            return None;
        }
        let mut sorted: Vec<i128> = values.iter().copied().collect();
        sorted.sort_unstable();
        let middle = sorted.len() / 2;
        if sorted.len() % 2 == 0 {
            Some(sorted[middle - 1] + sorted[middle])
        } else {
            Some(sorted[middle] * 2)
        }
    }

    fn handle_funding_boundary(&mut self, ctx: &mut Ctx, bar: &Bar, index: usize) {
        self.post_window = None;
        let Some((volume_sum, first_open, final_close)) = self.pre_boundary_stats() else {
            return;
        };

        let history = &self.prior_volumes[index];
        let enough_history = history.len() >= MIN_HISTORY;
        let volume_ok = Self::median_twice(history)
            .is_some_and(|median_twice| volume_sum * 20 >= median_twice * 13);
        let return_ok = i128::from(final_close) * 1_000 <= i128::from(first_open) * 996;
        let qualifies = enough_history
            && volume_ok
            && return_ok
            && ctx.position(INSTRUMENT).is_zero()
            && ctx.open_orders() == 0;

        let history = &mut self.prior_volumes[index];
        history.push_back(volume_sum);
        if history.len() > HISTORY_LIMIT {
            history.pop_front();
        }

        if qualifies {
            self.post_window = Some(PostWindow {
                boundary_ns: bar.close_ts.as_nanos(),
                first_open_raw: None,
                low_raw: i64::MAX,
            });
        }
    }

    fn process_post_window(&mut self, ctx: &mut Ctx, bar: &Bar) {
        let Some(mut window) = self.post_window.take() else {
            return;
        };
        let elapsed_ns = bar.close_ts.as_nanos() - window.boundary_ns;
        if elapsed_ns <= 0 || elapsed_ns % MINUTE_NS != 0 {
            return;
        }
        let elapsed_minutes = elapsed_ns / MINUTE_NS;
        if !(1..=15).contains(&elapsed_minutes) {
            return;
        }
        if elapsed_ns != elapsed_minutes * MINUTE_NS {
            return;
        }

        if elapsed_minutes == 1 {
            window.first_open_raw = Some(bar.open.raw());
        }
        window.low_raw = window.low_raw.min(bar.low.raw());

        if elapsed_minutes < 15 {
            self.post_window = Some(window);
            return;
        }

        let post_low_raw = window.low_raw;
        let rebound_ok = i128::from(bar.close.raw()) * 1_000
            >= i128::from(post_low_raw) * 1_002;
        let above_first_open = window
            .first_open_raw
            .is_some_and(|open| bar.close.raw() > open);

        if rebound_ok && above_first_open {
            self.submit_entry(ctx, bar, post_low_raw);
        }
    }

    fn submit_entry(&mut self, ctx: &mut Ctx, bar: &Bar, post_low_raw: i64) {
        if ctx.position(INSTRUMENT).is_positive() || ctx.open_orders() != 0 || !bar.close.is_positive() {
            return;
        }

        let equity_budget_raw = ctx.equity().raw() / 4;
        if equity_budget_raw <= 0 {
            return;
        }
        let target_qty_raw = (i128::from(equity_budget_raw) * RAW_SCALE)
            / i128::from(bar.close.raw());
        if target_qty_raw <= 0 {
            return;
        }
        let target_qty_raw = target_qty_raw.min(i128::from(i64::MAX)) as i64;
        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        let volume_cap = bar.volume.mul_ratio(1, 10);
        let mut qty_raw = target_qty_raw.min(affordable.raw()).min(volume_cap.raw());
        if qty_raw <= 0 {
            return;
        }

        self.entry_low_for_fill = Some(post_low_raw);
        let qty = Qty::from_raw(qty_raw);
        let _ = ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        // Keep the local value bounded even if future sizing changes.
        qty_raw = qty_raw.min(i64::MAX);
        let _ = qty_raw;
    }

    fn manage_position(&mut self, ctx: &mut Ctx, bar: &Bar) {
        let position = ctx.position(INSTRUMENT);
        if !position.is_positive() {
            if self.position_state.is_some() {
                self.position_state = None;
                self.entry_low_for_fill = None;
            }
            return;
        }
        if ctx.open_orders() != 0 {
            return;
        }

        let Some(mut state) = self.position_state else {
            return;
        };
        let close_raw = bar.close.raw();
        let below_post_low = close_raw < state.post_low_raw;
        let profit_target = i128::from(close_raw) * 1_000
            >= i128::from(state.entry_price_raw) * 1_008;
        let timed_out = bar.close_ts.as_nanos() - state.entry_ts_ns
            >= TimeDelta::from_mins(120).as_nanos();

        if below_post_low || profit_target || timed_out {
            state.exit_triggered = true;
        }
        self.position_state = Some(state);
        if !state.exit_triggered {
            return;
        }

        let volume_cap = bar.volume.mul_ratio(1, 10);
        let qty_raw = position.raw().min(volume_cap.raw());
        if qty_raw > 0 {
            let _ = ctx.submit(OrderRequest::market(
                INSTRUMENT,
                Side::Sell,
                Qty::from_raw(qty_raw),
            ));
        }
    }
}

impl Strategy for FundingWindowFade {
    fn name(&self) -> String {
        "funding-window-selloff-fade".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let ts_ns = bar.close_ts.as_nanos();
        self.bars.push_back(MinuteBar {
            ts_ns,
            open_raw: bar.open.raw(),
            low_raw: bar.low.raw(),
            close_raw: bar.close.raw(),
            volume_raw: bar.volume.raw(),
        });
        while self.bars.len() > 30 {
            self.bars.pop_front();
        }

        let minute_of_day = (ts_ns / MINUTE_NS) % 1_440;
        let funding_index = match minute_of_day {
            0 => Some(0),
            480 => Some(1),
            960 => Some(2),
            _ => None,
        };
        if let Some(index) = funding_index {
            self.handle_funding_boundary(ctx, bar, index);
        } else {
            self.process_post_window(ctx, bar);
        }

        self.manage_position(ctx, bar);
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        match fill.side {
            Side::Buy => {
                let low_raw = self.entry_low_for_fill.unwrap_or(fill.price.raw());
                if let Some(mut state) = self.position_state {
                    let old_weight = i128::from(state.entry_price_raw) * i128::from(state.qty_raw);
                    let new_weight = i128::from(fill.price.raw()) * i128::from(fill.qty.raw());
                    let new_qty = state.qty_raw.saturating_add(fill.qty.raw());
                    if new_qty > 0 {
                        state.entry_price_raw = ((old_weight + new_weight) / i128::from(new_qty)) as i64;
                        state.qty_raw = new_qty;
                        self.position_state = Some(state);
                    }
                } else {
                    self.position_state = Some(PositionState {
                        qty_raw: fill.qty.raw(),
                        entry_price_raw: fill.price.raw(),
                        entry_ts_ns: fill.ts.as_nanos(),
                        post_low_raw: low_raw,
                        exit_triggered: false,
                    });
                }
            }
            Side::Sell => {
                if let Some(mut state) = self.position_state {
                    state.qty_raw = state.qty_raw.saturating_sub(fill.qty.raw());
                    if state.qty_raw <= 0 {
                        self.position_state = None;
                        self.entry_low_for_fill = None;
                    } else {
                        self.position_state = Some(state);
                    }
                }
            }
        }
    }
}

export_strategy!(FundingWindowFade::new());
