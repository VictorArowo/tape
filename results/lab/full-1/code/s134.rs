use tape_guest::{export_strategy, Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

struct DailyEmaTrendState {
    active_day: Option<i64>,
    active_close_raw: i64,
    active_day_finalized: bool,
    ema_fast: Option<f64>,
    ema_slow: Option<f64>,
    // Some(true) means the latest completed-day comparison is bullish;
    // Some(false) means bearish; None means equal or not initialized.
    action: Option<bool>,
    tracked_position_raw: i64,
    completed_round_trips: u64,
}

impl DailyEmaTrendState {
    fn new() -> Self {
        Self {
            active_day: None,
            active_close_raw: 0,
            active_day_finalized: false,
            ema_fast: None,
            ema_slow: None,
            action: None,
            tracked_position_raw: 0,
            completed_round_trips: 0,
        }
    }

    fn complete_day(&mut self, close: Price) {
        let value = close.to_f64();
        let fast = match self.ema_fast {
            Some(previous) => previous + (2.0 / 4.0) * (value - previous),
            None => value,
        };
        let slow = match self.ema_slow {
            Some(previous) => previous + (2.0 / 11.0) * (value - previous),
            None => value,
        };
        self.ema_fast = Some(fast);
        self.ema_slow = Some(slow);

        self.action = if fast > slow {
            Some(true)
        } else if fast < slow {
            Some(false)
        } else {
            None
        };
    }

    fn minimum_qty(a: Qty, b: Qty) -> Qty {
        if a.raw() < b.raw() { a } else { b }
    }
}

impl Strategy for DailyEmaTrendState {
    fn name(&self) -> String {
        String::from("daily-ema-trend-state")
    }

    fn on_start(&mut self, ctx: &mut Ctx) {
        self.tracked_position_raw = ctx.position(INSTRUMENT).raw().max(0);
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let day_ns = TimeDelta::from_days(1).as_nanos();
        let open_day = bar.open_ts.as_nanos().div_euclid(day_ns);
        let close_day = bar.close_ts.as_nanos().div_euclid(day_ns);

        match self.active_day {
            None => {
                self.active_day = Some(open_day);
                self.active_close_raw = bar.close.raw();
                self.active_day_finalized = false;
            }
            Some(day) if day != open_day => {
                // Fallback for a missing midnight-crossing bar: use the last
                // observed close from the prior UTC day.
                if !self.active_day_finalized {
                    self.complete_day(Price::from_raw(self.active_close_raw));
                }
                self.active_day = Some(open_day);
                self.active_close_raw = bar.close.raw();
                self.active_day_finalized = false;
            }
            _ => {
                self.active_close_raw = bar.close.raw();
            }
        }

        // A one-minute bar starting on one UTC day and closing on the next is
        // the final bar of its starting day. Its close completes that day.
        if close_day > open_day && !self.active_day_finalized {
            self.active_close_raw = bar.close.raw();
            self.complete_day(bar.close);
            self.active_day_finalized = true;
        }

        if ctx.open_orders() != 0 {
            return;
        }

        let volume_cap = bar.volume.mul_ratio(1, 10);
        if !volume_cap.is_positive() {
            return;
        }

        match (self.action, ctx.position(INSTRUMENT)) {
            (Some(true), position) if !position.is_positive() => {
                let affordable = ctx.affordable_qty(
                    INSTRUMENT,
                    bar.close,
                    ctx.cash(),
                    SIZING_MARGIN_PPM,
                );
                let qty = Self::minimum_qty(affordable, volume_cap);
                if qty.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                }
            }
            (Some(false), position) if position.is_positive() => {
                let qty = Self::minimum_qty(position, volume_cap);
                if qty.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
                }
            }
            _ => {}
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        let before = self.tracked_position_raw;
        match fill.side {
            Side::Buy => {
                self.tracked_position_raw = self
                    .tracked_position_raw
                    .saturating_add(fill.qty.raw());
            }
            Side::Sell => {
                self.tracked_position_raw = self
                    .tracked_position_raw
                    .saturating_sub(fill.qty.raw())
                    .max(0);
                if before > 0 && self.tracked_position_raw == 0 {
                    self.completed_round_trips += 1;
                }
            }
        }
    }

    fn on_finish(&mut self, ctx: &mut Ctx) {
        let status = if self.completed_round_trips < 30 {
            "REJECT: fewer than 30 completed round trips"
        } else {
            "30-round-trip minimum met"
        };
        let message = format!(
            "daily-ema-trend-state: completed_round_trips={}, {}",
            self.completed_round_trips, status
        );
        ctx.log(&message);
    }
}

export_strategy!(DailyEmaTrendState::new());
