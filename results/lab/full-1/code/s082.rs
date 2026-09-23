use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, Fill, InstrumentId, OrderId, OrderRequest, OrderUpdate, Side, Strategy,
    TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const MINUTES_PER_DAY: i64 = 1_440;
const WINDOW_BARS: usize = 120;
const REQUIRED_HISTORY_DAYS: usize = 20;
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy)]
struct Sample {
    ts: i64,
    close_raw: i64,
    high_raw: i64,
    volume_raw: i64,
}

struct FundingWindowPressureRelease {
    bars: VecDeque<Sample>,
    // Each slot stores (UTC day number, prior-two-hour quote-volume proxy).
    window_volumes: [VecDeque<(i64, i128)>; 3],
    entry_price_raw: Option<i64>,
    entry_ts: Option<i64>,
    entry_qty_raw: i64,
    entry_cost_raw: i128,
    timed_exit_order: Option<OrderId>,
    cancel_pending: bool,
}

impl FundingWindowPressureRelease {
    fn new() -> Self {
        Self {
            bars: VecDeque::with_capacity(30_000),
            window_volumes: std::array::from_fn(|_| VecDeque::with_capacity(22)),
            entry_price_raw: None,
            entry_ts: None,
            entry_qty_raw: 0,
            entry_cost_raw: 0,
            timed_exit_order: None,
            cancel_pending: false,
        }
    }

    fn close_at(&self, ts: i64) -> Option<i64> {
        self.bars
            .iter()
            .rev()
            .find(|sample| sample.ts == ts)
            .map(|sample| sample.close_raw)
    }

    fn prior_window_stats(&self, now: i64, minute_ns: i64) -> Option<(i64, i128)> {
        if self.bars.len() < WINDOW_BARS {
            return None;
        }

        let mut highest_high = i64::MIN;
        let mut quote_volume = 0_i128;
        for offset in 0..WINDOW_BARS {
            let sample = self.bars[self.bars.len() - 1 - offset];
            let expected_ts = now.checked_sub((offset as i64).checked_mul(minute_ns)?)?;
            if sample.ts != expected_ts || sample.close_raw <= 0 || sample.volume_raw < 0 {
                return None;
            }
            highest_high = highest_high.max(sample.high_raw);
            // OHLCV supplies base volume, so close * volume is the quote-volume proxy.
            quote_volume += i128::from(sample.close_raw) * i128::from(sample.volume_raw);
        }

        (highest_high > 0).then_some((highest_high, quote_volume))
    }

    fn median_prior_volume(&mut self, slot: usize, day: i64) -> Option<i128> {
        let history = &mut self.window_volumes[slot];
        while let Some(&(old_day, _)) = history.front() {
            if old_day < day - REQUIRED_HISTORY_DAYS as i64 {
                history.pop_front();
            } else {
                break;
            }
        }

        let mut values: Vec<i128> = history
            .iter()
            .filter(|(sample_day, _)| *sample_day < day && day - *sample_day <= REQUIRED_HISTORY_DAYS as i64)
            .map(|(_, volume)| *volume)
            .collect();
        if values.len() != REQUIRED_HISTORY_DAYS {
            return None;
        }
        values.sort_unstable();
        Some((values[REQUIRED_HISTORY_DAYS / 2 - 1] + values[REQUIRED_HISTORY_DAYS / 2]) / 2)
    }

    fn record_event_volume(&mut self, slot: usize, day: i64, volume: i128) {
        let history = &mut self.window_volumes[slot];
        if let Some((last_day, last_volume)) = history.back_mut() {
            if *last_day == day {
                *last_volume = volume;
                return;
            }
        }
        history.push_back((day, volume));
        while history.len() > REQUIRED_HISTORY_DAYS + 1 {
            history.pop_front();
        }
    }

    fn event_signal(&mut self, slot: usize, day: i64, now: i64, minute_ns: i64) -> bool {
        let window = self.prior_window_stats(now, minute_ns);
        let median_volume = self.median_prior_volume(slot, day);
        let prior_close = now
            .checked_sub(MINUTES_PER_DAY * minute_ns)
            .and_then(|target| self.close_at(target));

        let signal = match (window, median_volume, prior_close) {
            (Some((highest_high, current_volume)), Some(median), Some(prior))
                if prior > 0 && median > 0 =>
            {
                let close = self.bars.back().map(|sample| sample.close_raw).unwrap_or(0);
                let return_ok = i128::from(close) * 100 >= i128::from(prior) * 101;
                let drawdown_ok = i128::from(close) * 1_000
                    <= i128::from(highest_high) * 996
                    && i128::from(close) * 1_000 >= i128::from(highest_high) * 985;
                let volume_ok = current_volume * 10 >= median * 12;
                return_ok && drawdown_ok && volume_ok
            }
            _ => false,
        };

        if let Some((_, volume)) = window {
            self.record_event_volume(slot, day, volume);
        }
        signal
    }

    fn clear_entry(&mut self) {
        self.entry_price_raw = None;
        self.entry_ts = None;
        self.entry_qty_raw = 0;
        self.entry_cost_raw = 0;
    }

    fn reconcile_orders(&mut self, ctx: &Ctx) {
        if ctx.open_orders() == 0 {
            self.timed_exit_order = None;
            self.cancel_pending = false;
        }
    }
}

impl Strategy for FundingWindowPressureRelease {
    fn name(&self) -> String {
        "funding-window-pressure-release".to_owned()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let now = bar.close_ts.as_nanos();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let day = now.div_euclid(day_ns);
        let minute_of_day = now.div_euclid(minute_ns).rem_euclid(MINUTES_PER_DAY);

        self.bars.push_back(Sample {
            ts: now,
            close_raw: bar.close.raw(),
            high_raw: bar.high.raw(),
            volume_raw: bar.volume.raw(),
        });
        while self.bars.len() > 30_000 {
            self.bars.pop_front();
        }

        let event_slot = match minute_of_day {
            10 => Some(0),
            490 => Some(1),
            970 => Some(2),
            _ => None,
        };
        let signal = event_slot
            .map(|slot| self.event_signal(slot, day, now, minute_ns))
            .unwrap_or(false);

        self.reconcile_orders(ctx);
        let position = ctx.position(INSTRUMENT);

        if position.is_positive() {
            if let (Some(entry_price), Some(entry_ts)) = (self.entry_price_raw, self.entry_ts) {
                let protective_exit = i128::from(bar.close.raw()) * 100
                    <= i128::from(entry_price) * 99;
                if protective_exit {
                    if let Some(order_id) = self.timed_exit_order.take() {
                        ctx.cancel(order_id);
                        self.cancel_pending = true;
                    }
                    if ctx.open_orders() == 0 {
                        self.cancel_pending = false;
                    }
                    if ctx.open_orders() == 0 && !self.cancel_pending {
                        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                    }
                    return;
                }

                if self.cancel_pending || ctx.open_orders() > 0 {
                    return;
                }
                let exit_due = now >= entry_ts.saturating_add(TimeDelta::from_mins(180).as_nanos());
                if exit_due && self.timed_exit_order.is_none() {
                    let order_id = ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                    self.timed_exit_order = Some(order_id);
                }
            }
            return;
        }

        if signal && ctx.open_orders() == 0 {
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Buy {
            let qty = fill.qty.raw().max(0);
            if qty == 0 {
                return;
            }
            if self.entry_qty_raw == 0 {
                self.entry_ts = Some(fill.ts.as_nanos());
            }
            self.entry_qty_raw = self.entry_qty_raw.saturating_add(qty);
            self.entry_cost_raw += i128::from(fill.price.raw()) * i128::from(qty);
            self.entry_price_raw = Some((self.entry_cost_raw / i128::from(self.entry_qty_raw)) as i64);
        } else if fill.side == Side::Sell {
            self.entry_qty_raw = self.entry_qty_raw.saturating_sub(fill.qty.raw().max(0));
            if self.entry_qty_raw == 0 {
                self.clear_entry();
            } else if let Some(price) = self.entry_price_raw {
                self.entry_cost_raw = i128::from(price) * i128::from(self.entry_qty_raw);
            }
            self.timed_exit_order = None;
            self.cancel_pending = false;
        }
    }

    fn on_order(&mut self, ctx: &mut Ctx, _update: &OrderUpdate) {
        self.reconcile_orders(ctx);
    }
}

export_strategy!(FundingWindowPressureRelease::new());
