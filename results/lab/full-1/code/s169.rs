use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const ENTRY_MINUTE: i64 = 12 * 60 + 59;
const EXIT_MINUTE: i64 = 20 * 60 + 59;
const US_START_MINUTE: i64 = 13 * 60;
const REST_START_MINUTE: i64 = 21 * 60;
const WINDOW_COUNT: usize = 20;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct ActiveWindow {
    start_minute: i64,
    last_minute: i64,
    start_price: f64,
    valid: bool,
}

struct SessionWindow {
    start_minute_of_day: i64,
    end_minute_of_day: i64,
    expected_span_minutes: i64,
    hours: f64,
    active: Option<ActiveWindow>,
    returns: VecDeque<f64>,
}

impl SessionWindow {
    fn new(start_minute_of_day: i64, end_minute_of_day: i64, hours: f64) -> Self {
        let expected_span_minutes = if end_minute_of_day >= start_minute_of_day {
            end_minute_of_day - start_minute_of_day
        } else {
            1_440 - start_minute_of_day + end_minute_of_day
        };
        Self {
            start_minute_of_day,
            end_minute_of_day,
            expected_span_minutes,
            hours,
            active: None,
            returns: VecDeque::with_capacity(WINDOW_COUNT),
        }
    }

    fn observe(
        &mut self,
        absolute_minute: i64,
        minute_of_day: i64,
        bar_open: Price,
        bar_close: Price,
    ) {
        if minute_of_day == self.start_minute_of_day {
            self.active = Some(ActiveWindow {
                start_minute: absolute_minute,
                last_minute: absolute_minute,
                start_price: bar_open.to_f64(),
                valid: true,
            });
            return;
        }

        let Some(active) = self.active.as_mut() else {
            return;
        };
        if absolute_minute != active.last_minute + 1 {
            active.valid = false;
        }
        active.last_minute = absolute_minute;

        if minute_of_day != self.end_minute_of_day {
            return;
        }

        let Some(completed) = self.active.take() else {
            return;
        };
        let end_price = bar_close.to_f64();
        if !completed.valid
            || absolute_minute - completed.start_minute != self.expected_span_minutes
            || completed.start_price <= 0.0
            || end_price <= 0.0
        {
            return;
        }

        let hourly_log_return = (end_price / completed.start_price).ln() / self.hours;
        if hourly_log_return.is_finite() {
            if self.returns.len() == WINDOW_COUNT {
                self.returns.pop_front();
            }
            self.returns.push_back(hourly_log_return);
        }
    }

    fn mean(&self) -> Option<f64> {
        if self.returns.len() != WINDOW_COUNT {
            return None;
        }
        Some(self.returns.iter().sum::<f64>() / WINDOW_COUNT as f64)
    }
}

struct SessionRelativeDrift {
    us_overlap: SessionWindow,
    remaining_hours: SessionWindow,
}

impl Strategy for SessionRelativeDrift {
    fn name(&self) -> String {
        "Session-Relative Drift".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let minute_nanos = TimeDelta::from_mins(1).as_nanos();
        let absolute_minute = bar.close_ts.as_nanos().div_euclid(minute_nanos);
        let minute_of_day = absolute_minute.rem_euclid(1_440);

        self.us_overlap.observe(
            absolute_minute,
            minute_of_day,
            bar.open,
            bar.close,
        );
        self.remaining_hours.observe(
            absolute_minute,
            minute_of_day,
            bar.open,
            bar.close,
        );

        if ctx.open_orders() != 0 {
            return;
        }

        if minute_of_day == EXIT_MINUTE {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
            return;
        }

        if minute_of_day != ENTRY_MINUTE || !ctx.position(INSTRUMENT).is_zero() {
            return;
        }

        let (Some(us_mean), Some(remaining_mean)) =
            (self.us_overlap.mean(), self.remaining_hours.mean())
        else {
            return;
        };

        if us_mean > remaining_mean {
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        }
    }
}

export_strategy!(SessionRelativeDrift {
    us_overlap: SessionWindow::new(US_START_MINUTE, EXIT_MINUTE, 8.0),
    remaining_hours: SessionWindow::new(REST_START_MINUTE, ENTRY_MINUTE, 16.0),
});
