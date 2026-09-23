use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, TimeInForce,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const FAST_SPAN: usize = 6;
const SLOW_SPAN: usize = 54;
const SIZING_MARGIN_PPM: i64 = 5_000;
const MINUTES_PER_FOUR_HOURS: usize = 240;

struct Ema {
    alpha: f64,
    value: Option<f64>,
}

impl Ema {
    fn new(span: usize) -> Self {
        Self {
            alpha: 2.0 / (span as f64 + 1.0),
            value: None,
        }
    }

    fn update(&mut self, value: f64) -> f64 {
        let next = match self.value {
            Some(previous) => self.alpha * value + (1.0 - self.alpha) * previous,
            None => value,
        };
        self.value = Some(next);
        next
    }
}

struct FourHourBucket {
    start_ns: i64,
    count: usize,
    valid: bool,
    finalized: bool,
    last_close_ns: Option<i64>,
    close: tape_guest::Price,
}

impl FourHourBucket {
    fn new(start_ns: i64) -> Self {
        Self {
            start_ns,
            count: 0,
            valid: true,
            finalized: false,
            last_close_ns: None,
            close: tape_guest::Price::from_raw(0),
        }
    }

    fn push(&mut self, bar: &Bar, minute_ns: i64, period_ns: i64) {
        let open_ns = bar.open_ts.as_nanos();
        let close_ns = bar.close_ts.as_nanos();
        let expected_end = self.start_ns.saturating_add(period_ns);

        if open_ns < self.start_ns
            || close_ns > expected_end
            || open_ns.checked_add(minute_ns) != Some(close_ns)
            || self.last_close_ns.is_some_and(|last| last != open_ns)
        {
            self.valid = false;
        }
        if self.count == 0 && open_ns != self.start_ns {
            self.valid = false;
        }

        self.count += 1;
        if self.count > MINUTES_PER_FOUR_HOURS {
            self.valid = false;
        }
        self.last_close_ns = Some(close_ns);
        self.close = bar.close;
    }

    fn is_complete(&self, period_ns: i64) -> bool {
        self.valid
            && self.count == MINUTES_PER_FOUR_HOURS
            && self.last_close_ns == Some(self.start_ns.saturating_add(period_ns))
    }
}

struct MultiDayEmaCrossover {
    fast: Ema,
    slow: Ema,
    previous_spread: Option<f64>,
    target_long: Option<bool>,
    bucket: Option<FourHourBucket>,
}

impl MultiDayEmaCrossover {
    fn on_completed_four_hour_bar(&mut self, close: tape_guest::Price) {
        let fast = self.fast.update(close.to_f64());
        let slow = self.slow.update(close.to_f64());
        let spread = fast - slow;

        if let Some(previous) = self.previous_spread {
            if previous <= 0.0 && spread > 0.0 {
                self.target_long = Some(true);
            } else if previous >= 0.0 && spread < 0.0 {
                self.target_long = Some(false);
            }
        }
        self.previous_spread = Some(spread);
    }

    fn trade_toward_target(&self, ctx: &mut Ctx, bar: &Bar) {
        let Some(target_long) = self.target_long else {
            return;
        };
        if ctx.open_orders() != 0 || !bar.open.is_positive() {
            return;
        }

        let volume_cap = Qty::from_raw(bar.volume.raw() / 10);
        if !volume_cap.is_positive() {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if target_long {
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                bar.open,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            let qty = if affordable < volume_cap {
                affordable
            } else {
                volume_cap
            };
            if qty.is_positive() {
                ctx.submit(
                    OrderRequest::market(INSTRUMENT, Side::Buy, qty)
                        .with_time_in_force(TimeInForce::Ioc),
                );
            }
        } else if position.is_positive() {
            let qty = if position < volume_cap {
                position
            } else {
                volume_cap
            };
            if qty.is_positive() {
                ctx.submit(
                    OrderRequest::market(INSTRUMENT, Side::Sell, qty)
                        .with_time_in_force(TimeInForce::Ioc),
                );
            }
        }
    }
}

impl Strategy for MultiDayEmaCrossover {
    fn name(&self) -> String {
        format!("multi-day-ema-crossover({FAST_SPAN},{SLOW_SPAN})")
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        // A target set by a completed 4-hour bar is acted on from this
        // subsequent one-minute bar onward. IOC orders cancel any remainder.
        self.trade_toward_target(ctx, bar);

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let period_ns = TimeDelta::from_hours(4).as_nanos();
        if period_ns <= 0 || minute_ns <= 0 {
            return;
        }

        let open_ns = bar.open_ts.as_nanos();
        let bucket_start = open_ns.div_euclid(period_ns) * period_ns;
        if self
            .bucket
            .as_ref()
            .is_none_or(|bucket| bucket.start_ns != bucket_start)
        {
            self.bucket = Some(FourHourBucket::new(bucket_start));
        }

        let Some(bucket) = self.bucket.as_mut() else {
            return;
        };
        bucket.push(bar, minute_ns, period_ns);

        let boundary_ns = bucket.start_ns.saturating_add(period_ns);
        if !bucket.finalized && bar.close_ts.as_nanos() == boundary_ns {
            bucket.finalized = true;
            if bucket.is_complete(period_ns) {
                let completed_close = bucket.close;
                self.on_completed_four_hour_bar(completed_close);
            }
        }
    }
}

export_strategy!(MultiDayEmaCrossover {
    fast: Ema::new(FAST_SPAN),
    slow: Ema::new(SLOW_SPAN),
    previous_spread: None,
    target_long: None,
    bucket: None,
});
