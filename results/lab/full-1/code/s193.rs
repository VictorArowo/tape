use tape_guest::{
    Bar, Ctx, InstrumentId, OrderId, OrderRequest, Qty, Side, Strategy, TimeDelta,
    TimeInForce, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

struct SeededEma {
    period: usize,
    count: usize,
    seed_sum: f64,
    value: Option<f64>,
}

impl SeededEma {
    fn new(period: usize) -> Self {
        Self {
            period,
            count: 0,
            seed_sum: 0.0,
            value: None,
        }
    }

    fn push(&mut self, price: f64) {
        if self.value.is_none() {
            self.count += 1;
            self.seed_sum += price;
            if self.count == self.period {
                self.value = Some(self.seed_sum / self.period as f64);
            }
            return;
        }

        let alpha = 2.0 / (self.period as f64 + 1.0);
        let previous = self.value.expect("initialized EMA");
        self.value = Some(alpha * price + (1.0 - alpha) * previous);
    }
}

struct MultiDayEmaRegime {
    ema_12: SeededEma,
    ema_48: SeededEma,
    target_long: Option<bool>,
    sizing_needed: bool,
    remaining: Qty,
    active_order: Option<OrderId>,
}

impl MultiDayEmaRegime {
    fn new() -> Self {
        Self {
            ema_12: SeededEma::new(12),
            ema_48: SeededEma::new(48),
            target_long: None,
            sizing_needed: false,
            remaining: Qty::from_raw(0),
            active_order: None,
        }
    }

    fn min_qty(a: Qty, b: Qty) -> Qty {
        Qty::from_raw(a.raw().min(b.raw()))
    }

    fn prepare_order(&mut self, ctx: &Ctx, price: tape_guest::Price) {
        let Some(target_long) = self.target_long else {
            return;
        };
        if !self.sizing_needed {
            return;
        }

        self.remaining = if target_long {
            let budget = ctx.cash().mul_ratio(997, 1_000);
            ctx.affordable_qty(INSTRUMENT, price, budget, SIZING_MARGIN_PPM)
        } else {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                position
            } else {
                Qty::from_raw(0)
            }
        };
        self.sizing_needed = false;
    }
}

impl Strategy for MultiDayEmaRegime {
    fn name(&self) -> String {
        "multi-day-ema-regime".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        if ctx.open_orders() == 0 {
            self.active_order = None;
        }

        let hour_ns = TimeDelta::from_hours(1).as_nanos();
        let is_hour_close = hour_ns > 0 && bar.close_ts.as_nanos().rem_euclid(hour_ns) == 0;
        let mut target_changed = false;

        if is_hour_close {
            let close = bar.close.to_f64();
            self.ema_12.push(close);
            self.ema_48.push(close);

            if let (Some(fast), Some(slow)) = (self.ema_12.value, self.ema_48.value) {
                let next_target = fast > slow;
                if self.target_long != Some(next_target) {
                    self.target_long = Some(next_target);
                    self.remaining = Qty::from_raw(0);
                    self.sizing_needed = true;
                    target_changed = true;
                    if let Some(order_id) = self.active_order.take() {
                        ctx.cancel(order_id);
                    }
                }
            }
        }

        if self.target_long.is_none()
            || target_changed
            || self.active_order.is_some()
            || ctx.open_orders() != 0
        {
            return;
        }

        self.prepare_order(ctx, bar.close);
        if !self.remaining.is_positive() {
            return;
        }

        let volume_cap = bar.volume.mul_ratio(1, 10);
        if !volume_cap.is_positive() {
            return;
        }

        let Some(target_long) = self.target_long else {
            return;
        };
        let requested = if target_long {
            Self::min_qty(self.remaining, volume_cap)
        } else {
            let position = ctx.position(INSTRUMENT);
            Self::min_qty(Self::min_qty(self.remaining, volume_cap), position)
        };
        if !requested.is_positive() {
            return;
        }

        let side = if target_long { Side::Buy } else { Side::Sell };
        let order = OrderRequest::market(INSTRUMENT, side, requested)
            .with_time_in_force(TimeInForce::Ioc);
        self.active_order = Some(ctx.submit(order));
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if self.active_order.is_some() {
            self.remaining = Qty::from_raw(
                self.remaining
                    .raw()
                    .saturating_sub(fill.qty.raw())
                    .max(0),
            );
        }
    }
}

export_strategy!(MultiDayEmaRegime::new());