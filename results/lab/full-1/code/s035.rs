use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const DAILY_WINDOW: usize = 20;

struct DailyTrendRegime {
    daily_closes: VecDeque<i64>,
    current_day: Option<i64>,
    current_close: Option<Price>,
    current_day_completed: bool,
}

impl DailyTrendRegime {
    fn new() -> Self {
        Self {
            daily_closes: VecDeque::with_capacity(DAILY_WINDOW),
            current_day: None,
            current_close: None,
            current_day_completed: false,
        }
    }

    fn record_daily_close(&mut self, close: Price) {
        self.daily_closes.push_back(close.raw());
        if self.daily_closes.len() > DAILY_WINDOW {
            self.daily_closes.pop_front();
        }
    }

    fn moving_average_sums(&self) -> Option<(i128, i128)> {
        if self.daily_closes.len() != DAILY_WINDOW {
            return None;
        }

        let sum_20: i128 = self.daily_closes.iter().map(|&p| i128::from(p)).sum();
        let sum_5: i128 = self
            .daily_closes
            .iter()
            .rev()
            .take(5)
            .map(|&p| i128::from(p))
            .sum();
        Some((sum_5, sum_20))
    }

    fn act_on_completed_day(&self, ctx: &mut Ctx, reference_price: Price) {
        let Some((sum_5, sum_20)) = self.moving_average_sums() else {
            return;
        };
        if sum_20 <= 0 || ctx.open_orders() != 0 {
            return;
        }

        // SMA5 / SMA20 = 4 * sum_5 / sum_20. Compare exactly against
        // the +0.25% entry and -0.25% exit thresholds.
        let position = ctx.position(INSTRUMENT);
        if position.is_zero() && 1_600 * sum_5 >= 401 * sum_20 {
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                reference_price,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        } else if position.is_positive() && 1_600 * sum_5 <= 399 * sum_20 {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
        }
    }
}

impl Strategy for DailyTrendRegime {
    fn name(&self) -> String {
        "daily-trend-regime".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let day_ns = TimeDelta::from_days(1).as_nanos();
        let open_day = bar.open_ts.as_nanos().div_euclid(day_ns);
        let close_day = bar.close_ts.as_nanos().div_euclid(day_ns);
        let mut completed_close = None;

        match self.current_day {
            None => {
                self.current_day = Some(open_day);
                self.current_close = Some(bar.close);
                self.current_day_completed = false;
            }
            Some(day) if open_day > day => {
                if !self.current_day_completed {
                    completed_close = self.current_close;
                }
                self.current_day = Some(open_day);
                self.current_close = Some(bar.close);
                self.current_day_completed = false;
            }
            Some(day) if open_day == day => {
                self.current_close = Some(bar.close);
            }
            Some(_) => return,
        }

        if let Some(close) = completed_close {
            self.record_daily_close(close);
            self.act_on_completed_day(ctx, bar.close);
        }

        // A bar whose interval ends on the next UTC date completes its open day.
        if close_day > open_day && !self.current_day_completed {
            self.record_daily_close(bar.close);
            self.current_day_completed = true;
            self.act_on_completed_day(ctx, bar.close);
        }
    }
}

export_strategy!(DailyTrendRegime::new());
