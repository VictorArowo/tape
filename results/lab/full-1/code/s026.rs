use tape_guest::{
    Bar, Ctx, InstrumentId, OrderId, OrderRequest, Price, Side, Strategy, TimeDelta,
    TimeInForce, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

struct Ema {
    alpha: f64,
    value: Option<f64>,
}

impl Ema {
    fn new(span: f64) -> Self {
        Self {
            alpha: 2.0 / (span + 1.0),
            value: None,
        }
    }

    fn update(&mut self, price: f64) -> f64 {
        let value = match self.value {
            Some(previous) => self.alpha * price + (1.0 - self.alpha) * previous,
            None => price,
        };
        self.value = Some(value);
        value
    }
}

struct HourlyEmaCrossover {
    fast: Ema,
    slow: Ema,
    previous_difference: Option<f64>,
    current_hour: Option<i64>,
    current_close: Option<Price>,
    target_long: bool,
    action_sent: bool,
    pending_order: Option<(OrderId, bool)>,
    cancellation_requested: bool,
}

impl HourlyEmaCrossover {
    fn finish_hour(&mut self, close: Price) {
        let price = close.to_f64();
        let fast = self.fast.update(price);
        let slow = self.slow.update(price);
        let difference = fast - slow;

        let crossed_up = self
            .previous_difference
            .is_some_and(|previous| previous <= 0.0 && difference > 0.0);
        let crossed_down = self
            .previous_difference
            .is_some_and(|previous| previous >= 0.0 && difference < 0.0);
        self.previous_difference = Some(difference);

        let new_target = if crossed_up {
            Some(true)
        } else if crossed_down {
            Some(false)
        } else {
            None
        };

        if let Some(target) = new_target {
            if target != self.target_long {
                self.target_long = target;
                self.action_sent = false;
            }
        }
    }

    fn update_hourly_bar(&mut self, bar: &Bar) {
        let hour_ns = TimeDelta::from_hours(1).as_nanos();
        let close_ns = bar.close_ts.as_nanos();
        // A bar ending exactly on the hour belongs to the hour just completed.
        let bucket = close_ns.saturating_sub(1).div_euclid(hour_ns);

        if self.current_hour.is_some_and(|hour| hour != bucket) {
            if let Some(close) = self.current_close.take() {
                self.finish_hour(close);
            }
            self.current_hour = None;
        }

        if self.current_hour.is_none() {
            self.current_hour = Some(bucket);
        }
        self.current_close = Some(bar.close);

        if close_ns.rem_euclid(hour_ns) == 0 && self.current_hour == Some(bucket) {
            if let Some(close) = self.current_close.take() {
                self.finish_hour(close);
            }
            self.current_hour = None;
        }
    }

    fn manage_target(&mut self, ctx: &mut Ctx, price: Price) {
        if ctx.open_orders() == 0 {
            self.pending_order = None;
            self.cancellation_requested = false;
        }

        if let Some((order_id, order_is_buy)) = self.pending_order {
            if order_is_buy != self.target_long {
                if !self.cancellation_requested {
                    ctx.cancel(order_id);
                    self.cancellation_requested = true;
                }
                return;
            }
            if self.cancellation_requested || ctx.open_orders() > 0 {
                return;
            }
        }

        if ctx.open_orders() > 0 || self.action_sent {
            return;
        }

        if self.target_long {
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                price,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                let id = ctx.submit(
                    OrderRequest::market(INSTRUMENT, Side::Buy, qty)
                        .with_time_in_force(TimeInForce::Gtc),
                );
                self.pending_order = Some((id, true));
            }
        } else {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                let id = ctx.submit(
                    OrderRequest::market(INSTRUMENT, Side::Sell, position)
                        .with_time_in_force(TimeInForce::Gtc),
                );
                self.pending_order = Some((id, false));
            }
        }
        self.action_sent = true;
    }
}

impl Strategy for HourlyEmaCrossover {
    fn name(&self) -> String {
        "Hourly EMA Crossover (12,72)".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        self.update_hourly_bar(bar);
        self.manage_target(ctx, bar.close);
    }
}

export_strategy!(HourlyEmaCrossover {
    fast: Ema::new(12.0),
    slow: Ema::new(72.0),
    previous_difference: None,
    current_hour: None,
    current_close: None,
    target_long: false,
    action_sent: false,
    pending_order: None,
    cancellation_requested: false,
});
