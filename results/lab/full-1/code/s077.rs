use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderId, OrderRequest, Side, Strategy, TimeDelta,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const DAILY_RETURNS: usize = 20;
const HOURLY_INTERVALS: usize = 24;
const HOLD_HOURS: i64 = 72;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct VolatilityScaledImpulseHold {
    hourly_closes: VecDeque<i64>,
    daily_returns: VecDeque<f64>,
    previous_daily_close: Option<i64>,
    entry_ts_nanos: Option<i64>,
    last_order_id: Option<OrderId>,
}

impl VolatilityScaledImpulseHold {
    fn record_hourly_close(&mut self, close: i64) {
        self.hourly_closes.push_back(close);
        while self.hourly_closes.len() > HOURLY_INTERVALS + 1 {
            self.hourly_closes.pop_front();
        }
    }

    fn record_daily_close(&mut self, close: i64) {
        if let Some(previous) = self.previous_daily_close {
            if previous > 0 && close > 0 {
                self.daily_returns
                    .push_back(close as f64 / previous as f64 - 1.0);
                while self.daily_returns.len() > DAILY_RETURNS {
                    self.daily_returns.pop_front();
                }
            }
        }
        self.previous_daily_close = Some(close);
    }

    fn trailing_24h_return(&self) -> Option<f64> {
        if self.hourly_closes.len() < HOURLY_INTERVALS + 1 {
            return None;
        }
        let oldest = *self.hourly_closes.front()?;
        let latest = *self.hourly_closes.back()?;
        (oldest > 0).then_some(latest as f64 / oldest as f64 - 1.0)
    }

    fn daily_return_stddev(&self) -> Option<f64> {
        if self.daily_returns.len() < DAILY_RETURNS {
            return None;
        }
        let count = self.daily_returns.len() as f64;
        let mean = self.daily_returns.iter().sum::<f64>() / count;
        let variance = self
            .daily_returns
            .iter()
            .map(|value| {
                let difference = value - mean;
                difference * difference
            })
            .sum::<f64>()
            / count;
        Some(variance.sqrt())
    }
}

impl Strategy for VolatilityScaledImpulseHold {
    fn name(&self) -> String {
        "volatility-scaled-impulse-hold".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let hour_ns = TimeDelta::from_hours(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let open_ns = bar.open_ts.as_nanos();
        let close_ns = bar.close_ts.as_nanos();
        let hourly_close = open_ns.div_euclid(hour_ns) != close_ns.div_euclid(hour_ns);
        let daily_close = open_ns.div_euclid(day_ns) != close_ns.div_euclid(day_ns);

        if daily_close {
            self.record_daily_close(bar.close.raw());
        }
        if hourly_close {
            self.record_hourly_close(bar.close.raw());
        }

        if ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            if let Some(entry_ns) = self.entry_ts_nanos {
                let hold_ns = TimeDelta::from_hours(HOLD_HOURS).as_nanos();
                if close_ns >= entry_ns.saturating_add(hold_ns) {
                    self.last_order_id = Some(ctx.submit(OrderRequest::market(
                        INSTRUMENT,
                        Side::Sell,
                        position,
                    )));
                }
            }
            return;
        }

        if !position.is_zero() || !hourly_close {
            return;
        }

        let (Some(return_24h), Some(daily_stddev)) =
            (self.trailing_24h_return(), self.daily_return_stddev())
        else {
            return;
        };

        if return_24h > 1.5 * daily_stddev {
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                self.last_order_id = Some(ctx.submit(OrderRequest::market(
                    INSTRUMENT,
                    Side::Buy,
                    qty,
                )));
            }
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        match fill.side {
            Side::Buy => self.entry_ts_nanos = Some(fill.ts.as_nanos()),
            Side::Sell => self.entry_ts_nanos = None,
        }
    }

    fn on_finish(&mut self, ctx: &mut Ctx) {
        if ctx.open_orders() != 0 {
            if let Some(order_id) = self.last_order_id.take() {
                ctx.cancel(order_id);
            }
        }

        if ctx.open_orders() == 0 {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                self.last_order_id = Some(ctx.submit(OrderRequest::market(
                    INSTRUMENT,
                    Side::Sell,
                    position,
                )));
            }
        }
    }
}

export_strategy!(VolatilityScaledImpulseHold {
    hourly_closes: VecDeque::with_capacity(HOURLY_INTERVALS + 1),
    daily_returns: VecDeque::with_capacity(DAILY_RETURNS),
    previous_daily_close: None,
    entry_ts_nanos: None,
    last_order_id: None,
});
