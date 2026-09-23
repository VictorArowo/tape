use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const LOSS_THRESHOLD: f64 = -0.08;
const PAUSE_DAYS: i64 = 3;

struct DownsideGapRiskPause {
    current_day: Option<i64>,
    current_day_close: Option<i64>,
    last_completed_close: Option<i64>,
    resume_day: Option<i64>,
    started: bool,
}

impl DownsideGapRiskPause {
    fn new() -> Self {
        Self {
            current_day: None,
            current_day_close: None,
            last_completed_close: None,
            resume_day: None,
            started: false,
        }
    }

    fn submit_buy(&self, ctx: &mut Ctx, bar: &Bar) {
        let qty = ctx.affordable_qty(INSTRUMENT, bar.close, ctx.cash(), SIZING_MARGIN_PPM);
        if qty.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }
}

impl Strategy for DownsideGapRiskPause {
    fn name(&self) -> String {
        "Downside Gap Risk Pause".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let day_nanos = TimeDelta::from_days(1).as_nanos();
        let day = bar.close_ts.as_nanos() / day_nanos;

        if !self.started {
            self.started = true;
            self.current_day = Some(day);
            self.current_day_close = Some(bar.close.raw());
            if ctx.open_orders() == 0 && ctx.position(INSTRUMENT).is_zero() {
                self.submit_buy(ctx, bar);
            }
            return;
        }

        let mut triggered = false;
        if let Some(active_day) = self.current_day {
            if day != active_day {
                if let Some(completed_close) = self.current_day_close {
                    if let Some(previous_close) = self.last_completed_close {
                        if previous_close > 0 {
                            let change = (completed_close as f64 - previous_close as f64)
                                / previous_close as f64;
                            if change <= LOSS_THRESHOLD {
                                triggered = true;
                                // The boundary day plus the following two complete days
                                // are paused; re-entry is on the subsequent day.
                                self.resume_day = Some(day + PAUSE_DAYS + 1);
                            }
                        }
                    }
                    self.last_completed_close = Some(completed_close);
                }
                self.current_day = Some(day);
                self.current_day_close = Some(bar.close.raw());
            } else {
                self.current_day_close = Some(bar.close.raw());
            }
        } else {
            self.current_day = Some(day);
            self.current_day_close = Some(bar.close.raw());
        }

        if ctx.open_orders() != 0 {
            return;
        }

        if triggered {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
            return;
        }

        if let Some(resume_day) = self.resume_day {
            if day >= resume_day {
                if ctx.position(INSTRUMENT).is_zero() {
                    self.submit_buy(ctx, bar);
                }
                self.resume_day = None;
            }
            return;
        }

        // Retry an initial entry if it could not be placed, without duplicating
        // an outstanding order.
        if ctx.position(INSTRUMENT).is_zero() {
            self.submit_buy(ctx, bar);
        }
    }
}

export_strategy!(DownsideGapRiskPause::new());