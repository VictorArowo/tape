use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, Money, OrderId, OrderRequest, Price, Qty, Side, Strategy,
    TimeDelta, TimeInForce, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const EMA_BARS: usize = 43_200;
const RETURN_BARS: usize = 360;
const HISTORY_LEN: usize = RETURN_BARS + 1;
const SCALE: i128 = 100_000_000;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct PassiveDipInventoryCapture {
    ema: Option<f64>,
    ema_bars_seen: usize,
    closes: VecDeque<i64>,

    buy_order: Option<OrderId>,
    buy_submitted_ns: Option<i64>,
    take_profit_order: Option<OrderId>,
    force_exit: bool,

    entry_ts_ns: Option<i64>,
    entry_qty_raw: i128,
    entry_cost_raw: i128,
    trade_net_raw: i128,
    trade_fees_raw: i128,

    completed_round_trips: u64,
    completed_net_raw: i128,
    completed_fees_raw: i128,
}

impl PassiveDipInventoryCapture {
    fn new() -> Self {
        Self {
            ema: None,
            ema_bars_seen: 0,
            closes: VecDeque::with_capacity(HISTORY_LEN),
            buy_order: None,
            buy_submitted_ns: None,
            take_profit_order: None,
            force_exit: false,
            entry_ts_ns: None,
            entry_qty_raw: 0,
            entry_cost_raw: 0,
            trade_net_raw: 0,
            trade_fees_raw: 0,
            completed_round_trips: 0,
            completed_net_raw: 0,
            completed_fees_raw: 0,
        }
    }

    fn cancel_buy(&mut self, ctx: &mut Ctx) {
        if let Some(order_id) = self.buy_order.take() {
            ctx.cancel(order_id);
        }
        self.buy_submitted_ns = None;
    }

    fn cancel_take_profit(&mut self, ctx: &mut Ctx) {
        if let Some(order_id) = self.take_profit_order.take() {
            ctx.cancel(order_id);
        }
    }

    fn entry_price(&self) -> Option<Price> {
        if self.entry_qty_raw <= 0 {
            return None;
        }
        let raw = self.entry_cost_raw * SCALE / self.entry_qty_raw;
        (raw > 0 && raw <= i128::from(i64::MAX)).then(|| Price::from_raw(raw as i64))
    }

    fn refresh_take_profit(&mut self, ctx: &mut Ctx) {
        self.cancel_take_profit(ctx);
        if self.force_exit {
            return;
        }
        let position = ctx.position(INSTRUMENT);
        let Some(entry_price) = self.entry_price() else {
            return;
        };
        if position.is_positive() {
            let target = entry_price.mul_ratio(1_014, 1_000);
            self.take_profit_order = Some(ctx.submit(
                OrderRequest::limit(INSTRUMENT, Side::Sell, position, target)
                    .with_time_in_force(TimeInForce::Gtc),
            ));
        }
    }

    fn reset_open_trade(&mut self, ctx: &mut Ctx) {
        self.cancel_take_profit(ctx);
        self.cancel_buy(ctx);
        self.force_exit = false;
        self.entry_ts_ns = None;
        self.entry_qty_raw = 0;
        self.entry_cost_raw = 0;
        self.trade_net_raw = 0;
        self.trade_fees_raw = 0;
    }

    fn quantity_for_budget(budget: Money, price: Price) -> Qty {
        if !budget.is_positive() || !price.is_positive() {
            return Qty::from_raw(0);
        }
        let raw = i128::from(budget.raw()) * SCALE / i128::from(price.raw());
        if raw <= 0 {
            return Qty::from_raw(0);
        }
        Qty::from_raw(raw.min(i128::from(i64::MAX)) as i64)
    }

    fn record_buy_fill(&mut self, fill_price: Price, fill_qty: Qty, fee: Money, ts_ns: i64) {
        if self.entry_ts_ns.is_none() {
            self.entry_ts_ns = Some(ts_ns);
        }
        self.entry_qty_raw += i128::from(fill_qty.raw());
        self.entry_cost_raw +=
            i128::from(fill_price.raw()) * i128::from(fill_qty.raw()) / SCALE;

        let notional_raw = (fill_price * fill_qty).raw();
        self.trade_net_raw -= i128::from(notional_raw) + i128::from(fee.raw());
        self.trade_fees_raw += i128::from(fee.raw());
    }

    fn record_sell_fill(&mut self, fill_price: Price, fill_qty: Qty, fee: Money) {
        let notional_raw = (fill_price * fill_qty).raw();
        self.trade_net_raw += i128::from(notional_raw) - i128::from(fee.raw());
        self.trade_fees_raw += i128::from(fee.raw());

        let sold_raw = i128::from(fill_qty.raw());
        if self.entry_qty_raw > 0 {
            let allocated_cost = if sold_raw >= self.entry_qty_raw {
                self.entry_cost_raw
            } else {
                self.entry_cost_raw * sold_raw / self.entry_qty_raw
            };
            self.entry_cost_raw -= allocated_cost;
            self.entry_qty_raw = (self.entry_qty_raw - sold_raw).max(0);
        }
    }

    fn expire_buy_if_needed(&mut self, ctx: &mut Ctx, now_ns: i64) {
        let Some(submitted_ns) = self.buy_submitted_ns else {
            return;
        };
        if now_ns.saturating_sub(submitted_ns) >= TimeDelta::from_hours(4).as_nanos() {
            self.cancel_buy(ctx);
        }
    }
}

impl Strategy for PassiveDipInventoryCapture {
    fn name(&self) -> String {
        "passive-dip-inventory-capture".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let close = bar.close.to_f64();
        let alpha = 2.0 / (EMA_BARS as f64 + 1.0);
        self.ema = Some(match self.ema {
            Some(previous) => previous + alpha * (close - previous),
            None => close,
        });
        self.ema_bars_seen = self.ema_bars_seen.saturating_add(1);

        self.closes.push_back(bar.close.raw());
        while self.closes.len() > HISTORY_LEN {
            self.closes.pop_front();
        }

        let now_ns = bar.close_ts.as_nanos();
        self.expire_buy_if_needed(ctx, now_ns);

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            let stop_hit = self.entry_price().is_some_and(|entry| {
                bar.low <= entry.mul_ratio(975, 1_000)
            });
            let time_exit = self.entry_ts_ns.is_some_and(|entry_ns| {
                now_ns.saturating_sub(entry_ns) >= TimeDelta::from_hours(24).as_nanos()
            });

            if self.force_exit || stop_hit || time_exit {
                self.force_exit = true;
                self.cancel_take_profit(ctx);
                self.cancel_buy(ctx);
                if ctx.open_orders() == 0 {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                }
            }
            return;
        }

        if self.entry_ts_ns.is_some() {
            self.reset_open_trade(ctx);
        }

        if self.buy_order.is_some() || ctx.open_orders() != 0 {
            return;
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        if minute_ns <= 0 {
            return;
        }
        let minute_of_day = (now_ns / minute_ns).rem_euclid(1_440);
        if minute_of_day % 60 != 0 || self.ema_bars_seen < EMA_BARS {
            return;
        }

        let Some(ema) = self.ema else {
            return;
        };
        let Some(prior_raw) = self.closes.front().copied().filter(|_| self.closes.len() == HISTORY_LEN)
        else {
            return;
        };
        if prior_raw <= 0 {
            return;
        }

        let trailing_return = bar.close.raw() as f64 / prior_raw as f64 - 1.0;
        if bar.close.to_f64() <= ema || !(-0.03..=-0.005).contains(&trailing_return) {
            return;
        }

        let limit_price = bar.close.mul_ratio(9_925, 10_000);
        if !limit_price.is_positive() {
            return;
        }
        let budget = ctx.equity().mul_ratio(1, 5);
        let target_qty = Self::quantity_for_budget(budget, limit_price);
        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            limit_price,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        let qty_raw = target_qty.raw().min(affordable.raw());
        if qty_raw <= 0 {
            return;
        }

        let order_id = ctx.submit(
            OrderRequest::limit(
                INSTRUMENT,
                Side::Buy,
                Qty::from_raw(qty_raw),
                limit_price,
            )
            .with_time_in_force(TimeInForce::Gtc),
        );
        self.buy_order = Some(order_id);
        self.buy_submitted_ns = Some(now_ns);
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.side == Side::Buy {
            self.record_buy_fill(fill.price, fill.qty, fill.fee, fill.ts.as_nanos());
            if self.force_exit {
                self.cancel_take_profit(ctx);
                self.cancel_buy(ctx);
            } else {
                self.refresh_take_profit(ctx);
            }
            return;
        }

        self.record_sell_fill(fill.price, fill.qty, fill.fee);
        self.cancel_buy(ctx);

        if ctx.position(INSTRUMENT).is_zero() {
            self.completed_round_trips = self.completed_round_trips.saturating_add(1);
            self.completed_net_raw += self.trade_net_raw;
            self.completed_fees_raw += self.trade_fees_raw;
            self.reset_open_trade(ctx);
        }
    }

    fn on_finish(&mut self, ctx: &mut Ctx) {
        let net = self.completed_net_raw as f64 / SCALE as f64;
        let fees = self.completed_fees_raw as f64 / SCALE as f64;
        ctx.log(&format!(
            "completed round trips: {}; net realized P&L: {:.8}; fees: {:.8}",
            self.completed_round_trips, net, fees
        ));
    }
}

export_strategy!(PassiveDipInventoryCapture::new());
