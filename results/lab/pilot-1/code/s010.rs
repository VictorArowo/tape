use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy)]
struct DayBar {
    day: i64,
    open: i64,
    close: i64,
}

struct PanicDayStagedReentry {
    current_day: Option<DayBar>,
    previous_day_close: Option<i64>,
    panic_exit_pending: bool,
    recovery_days: u8,
}

impl PanicDayStagedReentry {
    fn volume_cap(bar: &Bar) -> Qty {
        bar.volume.mul_ratio(1, 10)
    }

    fn finish_day(&mut self, completed: DayBar, ctx: &mut Ctx) {
        let position = ctx.position(INSTRUMENT);
        let prior_close = self.previous_day_close;

        if position.is_positive()
            && prior_close.is_some_and(|close| {
                i128::from(completed.close) * 100
                    <= i128::from(close) * 92
            })
        {
            self.panic_exit_pending = true;
            self.recovery_days = 0;
        } else if position.is_zero() && !self.panic_exit_pending {
            if completed.close > completed.open {
                self.recovery_days = self.recovery_days.saturating_add(1).min(2);
            } else {
                self.recovery_days = 0;
            }
        }

        self.previous_day_close = Some(completed.close);
    }

    fn submit_exit_if_possible(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if !self.panic_exit_pending || ctx.open_orders() != 0 {
            return;
        }
        let position = ctx.position(INSTRUMENT);
        if !position.is_positive() {
            self.panic_exit_pending = false;
            return;
        }
        let qty = if position < Self::volume_cap(bar) {
            position
        } else {
            Self::volume_cap(bar)
        };
        if qty.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
        }
    }

    fn submit_recovery_buy(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if self.panic_exit_pending
            || self.recovery_days == 0
            || ctx.open_orders() != 0
            || ctx.position(INSTRUMENT).is_positive()
        {
            return;
        }

        let cash = ctx.cash();
        let budget = if self.recovery_days == 1 {
            cash.mul_ratio(1, 2)
        } else {
            cash
        };
        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            bar.open,
            budget,
            SIZING_MARGIN_PPM,
        );
        let cap = Self::volume_cap(bar);
        let qty = if affordable < cap { affordable } else { cap };
        if qty.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }
}

impl Strategy for PanicDayStagedReentry {
    fn name(&self) -> String {
        "panic-day-staged-reentry".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let day = bar.close_ts.as_nanos() / tape_guest::TimeDelta::from_days(1).as_nanos();
        let mut completed_day = None;
        match self.current_day {
            None => {
                self.current_day = Some(DayBar {
                    day,
                    open: bar.open.raw(),
                    close: bar.close.raw(),
                });
            }
            Some(mut current) if current.day == day => {
                current.close = bar.close.raw();
                self.current_day = Some(current);
            }
            Some(current) => {
                completed_day = Some(current);
                self.current_day = Some(DayBar {
                    day,
                    open: bar.open.raw(),
                    close: bar.close.raw(),
                });
            }
        }

        if let Some(completed) = completed_day {
            self.finish_day(completed, ctx);
        }

        // Orders submitted here are intended for the next available one-minute open.
        if self.panic_exit_pending {
            self.submit_exit_if_possible(ctx, bar);
        } else if ctx.position(INSTRUMENT).is_zero() {
            self.submit_recovery_buy(ctx, bar);
        }
    }
}

export_strategy!(PanicDayStagedReentry {
    current_day: None,
    previous_day_close: None,
    panic_exit_pending: false,
    recovery_days: 0,
});