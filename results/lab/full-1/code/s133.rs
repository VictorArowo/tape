use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, TimeInForce,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const FAST_PERIOD: usize = 1_440;
const SLOW_PERIOD: usize = 7_200;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct Ema {
    period: usize,
    seen: usize,
    seed_sum: i128,
    value: Option<f64>,
}

impl Ema {
    fn new(period: usize) -> Self {
        Self {
            period,
            seen: 0,
            seed_sum: 0,
            value: None,
        }
    }

    fn push(&mut self, raw_price: i64) -> Option<f64> {
        let price = raw_price as f64;
        if let Some(previous) = self.value {
            let alpha = 2.0 / (self.period as f64 + 1.0);
            let value = alpha * price + (1.0 - alpha) * previous;
            self.value = Some(value);
            return self.value;
        }

        self.seen += 1;
        self.seed_sum += i128::from(raw_price);
        if self.seen == self.period {
            self.value = Some(self.seed_sum as f64 / self.period as f64);
        }
        self.value
    }
}

struct TwoHorizonTrendState {
    fast: Ema,
    slow: Ema,
}

impl Strategy for TwoHorizonTrendState {
    fn name(&self) -> String {
        String::from("two-horizon-trend-state")
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let fast = self.fast.push(bar.close.raw());
        let slow = self.slow.push(bar.close.raw());

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let minute_of_hour = (bar.close_ts.as_nanos() / minute_ns).rem_euclid(60);
        if minute_of_hour != 0 {
            return;
        }

        let (Some(fast), Some(slow)) = (fast, slow) else {
            return;
        };
        if ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_zero() {
            if fast > slow && bar.close.raw() as f64 > fast {
                let qty = ctx.affordable_qty(
                    INSTRUMENT,
                    bar.close,
                    ctx.cash(),
                    SIZING_MARGIN_PPM,
                );
                if qty.is_positive() {
                    ctx.submit(
                        OrderRequest::market(INSTRUMENT, Side::Buy, qty)
                            .with_time_in_force(TimeInForce::Ioc),
                    );
                }
            }
        } else if position.is_positive() && fast <= slow {
            ctx.submit(
                OrderRequest::market(INSTRUMENT, Side::Sell, position)
                    .with_time_in_force(TimeInForce::Ioc),
            );
        }
    }
}

export_strategy!(TwoHorizonTrendState {
    fast: Ema::new(FAST_PERIOD),
    slow: Ema::new(SLOW_PERIOD),
});
