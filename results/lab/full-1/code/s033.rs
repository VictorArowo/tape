use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta,
    TimeInForce, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const EMA_LEN: usize = 1_440;
const EMA_LOOKBACK: usize = 360;
const ATR_LEN: usize = 60;
const VOL_WINDOW: usize = 43_200;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct EntryOrder {
    id: OrderId,
    submitted_ns: i64,
}

struct PassiveTrendPullbacks {
    ema: Option<f64>,
    ema_history: VecDeque<f64>,
    bar_count: usize,
    previous_close: Option<f64>,
    true_ranges: VecDeque<f64>,
    true_range_sum: f64,
    volatility_history: VecDeque<(i64, f64)>,
    entry_order: Option<EntryOrder>,
    entry_price: Option<Price>,
    entry_qty: Qty,
    held_qty: Qty,
    held_since_ns: Option<i64>,
    target_start_ns: Option<i64>,
    target_order: Option<OrderId>,
    exit_requested: bool,
}

impl PassiveTrendPullbacks {
    fn new() -> Self {
        Self {
            ema: None,
            ema_history: VecDeque::with_capacity(EMA_LOOKBACK + 1),
            bar_count: 0,
            previous_close: None,
            true_ranges: VecDeque::with_capacity(ATR_LEN + 1),
            true_range_sum: 0.0,
            volatility_history: VecDeque::with_capacity(VOL_WINDOW + 1),
            entry_order: None,
            entry_price: None,
            entry_qty: Qty::from_raw(0),
            held_qty: Qty::from_raw(0),
            held_since_ns: None,
            target_start_ns: None,
            target_order: None,
            exit_requested: false,
        }
    }

    fn update_indicators(&mut self, bar: &Bar) -> Option<f64> {
        let close = bar.close.to_f64();
        let alpha = 2.0 / (EMA_LEN as f64 + 1.0);
        let ema = match self.ema {
            Some(previous) => previous + alpha * (close - previous),
            None => close,
        };
        self.ema = Some(ema);
        self.bar_count = self.bar_count.saturating_add(1);
        self.ema_history.push_back(ema);
        if self.ema_history.len() > EMA_LOOKBACK + 1 {
            self.ema_history.pop_front();
        }

        let high = bar.high.to_f64();
        let low = bar.low.to_f64();
        let true_range = match self.previous_close {
            Some(previous) => (high - low)
                .max((high - previous).abs())
                .max((low - previous).abs()),
            None => high - low,
        };
        self.previous_close = Some(close);
        self.true_ranges.push_back(true_range);
        self.true_range_sum += true_range;
        if self.true_ranges.len() > ATR_LEN {
            self.true_range_sum -= self.true_ranges.pop_front().unwrap_or(0.0);
        }

        if self.true_ranges.len() == ATR_LEN && close > 0.0 {
            Some((self.true_range_sum / ATR_LEN as f64) / close)
        } else {
            None
        }
    }

    fn conditions_hold(&self, current_volatility: Option<f64>) -> bool {
        let trend_holds = self.bar_count >= EMA_LEN
            && self.ema_history.len() == EMA_LOOKBACK + 1
            && self
                .ema
                .zip(self.ema_history.front().copied())
                .is_some_and(|(current, earlier)| current > earlier);

        let volatility_holds = current_volatility.is_some_and(|current| {
            if self.volatility_history.len() < VOL_WINDOW {
                return false;
            }
            let mut prior: Vec<f64> = self
                .volatility_history
                .iter()
                .map(|(_, value)| *value)
                .collect();
            prior.sort_by(f64::total_cmp);
            let rank = (3 * prior.len() + 3) / 4;
            let percentile = prior[rank.saturating_sub(1)];
            current < percentile
        });

        trend_holds && volatility_holds
    }

    fn clear_trade(&mut self) {
        self.entry_price = None;
        self.entry_qty = Qty::from_raw(0);
        self.held_qty = Qty::from_raw(0);
        self.held_since_ns = None;
        self.target_start_ns = None;
        self.target_order = None;
        self.exit_requested = false;
    }

    fn next_complete_bar_ns(ts_ns: i64) -> i64 {
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        (ts_ns / minute_ns + 1) * minute_ns
    }
}

impl Strategy for PassiveTrendPullbacks {
    fn name(&self) -> String {
        "passive-trend-pullbacks [wasm]".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let now_ns = bar.close_ts.as_nanos();
        let current_volatility = self.update_indicators(bar);
        let window_ns = TimeDelta::from_days(30).as_nanos();
        let cutoff_ns = now_ns - window_ns;
        while self
            .volatility_history
            .front()
            .is_some_and(|(ts, _)| *ts < cutoff_ns)
        {
            self.volatility_history.pop_front();
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let minute_index = now_ns / minute_ns;
        let decision_bar = minute_index % 15 == 0;
        let eligible = decision_bar && self.conditions_hold(current_volatility);

        if self.entry_order.as_ref().is_some_and(|order| {
            ctx.now().as_nanos() - order.submitted_ns >= TimeDelta::from_mins(30).as_nanos()
        }) {
            if let Some(order) = self.entry_order.take() {
                ctx.cancel(order.id);
            }
        }

        if decision_bar && !eligible {
            if let Some(order) = self.entry_order.take() {
                ctx.cancel(order.id);
            }
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() && self.entry_price.is_some() {
            let after_entry_bar = self
                .target_start_ns
                .is_some_and(|start| now_ns >= start);
            let stop_price = self.entry_price.map(|price| price.mul_ratio(9_930, 10_000));
            let stop_hit = after_entry_bar
                && stop_price.is_some_and(|price| bar.close <= price);
            let timed_out = self.held_since_ns.is_some_and(|held| {
                now_ns - held >= TimeDelta::from_hours(6).as_nanos()
            });

            if stop_hit || timed_out {
                self.exit_requested = true;
                if let Some(target) = self.target_order.take() {
                    ctx.cancel(target);
                }
            }

            if self.exit_requested {
                if ctx.open_orders() == 0 {
                    let current_position = ctx.position(INSTRUMENT);
                    if current_position.is_positive() {
                        ctx.submit(OrderRequest::market(
                            INSTRUMENT,
                            Side::Sell,
                            current_position,
                        ));
                    }
                }
            } else if after_entry_bar
                && self.target_order.is_none()
                && ctx.open_orders() == 0
            {
                if let Some(entry) = self.entry_price {
                    let target = entry.mul_ratio(10_070, 10_000);
                    ctx.submit(
                        OrderRequest::limit(INSTRUMENT, Side::Sell, position, target)
                            .with_time_in_force(TimeInForce::Gtc),
                    );
                    // The target ID is assigned from the returned order below.
                    // Keeping it lets stop/timeout handling cancel the resting order.
                    self.target_order = Some(ctx.submit(OrderRequest::limit(
                        INSTRUMENT,
                        Side::Sell,
                        Qty::from_raw(0),
                        target,
                    )));
                }
            }
        }

        if decision_bar
            && eligible
            && ctx.position(INSTRUMENT).is_zero()
            && self.entry_price.is_none()
            && self.entry_order.is_none()
            && ctx.open_orders() == 0
        {
            let limit_price = bar.close.mul_ratio(9_965, 10_000);
            let half_equity = ctx.equity().mul_ratio(1, 2);
            let budget = if ctx.cash() < half_equity {
                ctx.cash()
            } else {
                half_equity
            };
            if budget.is_positive() {
                let affordable = ctx.affordable_qty(
                    INSTRUMENT,
                    limit_price,
                    budget,
                    SIZING_MARGIN_PPM,
                );
                let volume_cap = bar.volume.mul_ratio(1, 10);
                let qty = if affordable < volume_cap {
                    affordable
                } else {
                    volume_cap
                };
                if qty.is_positive() {
                    let id = ctx.submit(
                        OrderRequest::limit(INSTRUMENT, Side::Buy, qty, limit_price)
                            .with_time_in_force(TimeInForce::Gtc),
                    );
                    self.entry_order = Some(EntryOrder {
                        id,
                        submitted_ns: ctx.now().as_nanos(),
                    });
                }
            }
        }

        if let Some(volatility) = current_volatility {
            self.volatility_history.push_back((now_ns, volatility));
            while self.volatility_history.len() > VOL_WINDOW {
                self.volatility_history.pop_front();
            }
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        let is_entry = self
            .entry_order
            .as_ref()
            .is_some_and(|order| order.id == fill.order_id);

        if is_entry && fill.side == Side::Buy {
            if let Some(order) = self.entry_order.take() {
                ctx.cancel(order.id);
            }
            let old_qty = self.entry_qty.raw();
            let added_qty = fill.qty.raw();
            let total_qty = old_qty + added_qty;
            if total_qty > 0 {
                let weighted_raw = (i128::from(self.entry_price.map_or(0, Price::raw))
                    * i128::from(old_qty)
                    + i128::from(fill.price.raw()) * i128::from(added_qty))
                    / i128::from(total_qty);
                self.entry_price = Some(Price::from_raw(weighted_raw as i64));
                self.entry_qty = Qty::from_raw(total_qty);
                self.held_qty = Qty::from_raw(self.held_qty.raw() + added_qty);
            }
            if self.held_since_ns.is_none() {
                self.held_since_ns = Some(fill.ts.as_nanos());
                self.target_start_ns = Some(Self::next_complete_bar_ns(fill.ts.as_nanos()));
            }
            self.exit_requested = false;
        } else if fill.side == Side::Sell {
            let remaining = self.held_qty.raw().saturating_sub(fill.qty.raw()).max(0);
            self.held_qty = Qty::from_raw(remaining);
            if remaining == 0 || ctx.position(INSTRUMENT).is_zero() {
                if let Some(target) = self.target_order.take() {
                    ctx.cancel(target);
                }
                self.clear_trade();
            }
        }
    }
}

export_strategy!(PassiveTrendPullbacks::new());
