use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SHORT_DAYS: usize = 30;
const LONG_DAYS: usize = 90;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct DailyCloses {
    closes: VecDeque<i64>,
    sum: i128,
}

impl DailyCloses {
    fn new() -> Self {
        Self {
            closes: VecDeque::with_capacity(LONG_DAYS + 1),
            sum: 0,
        }
    }

    fn push(&mut self, close: i64) {
        self.closes.push_back(close);
        self.sum += i128::from(close);
        if self.closes.len() > LONG_DAYS {
            self.sum -= i128::from(self.closes.pop_front().expect("non-empty"));
        }
    }

    fn averages_ready(&self) -> bool {
        self.closes.len() == LONG_DAYS
    }

    fn short_sum(&self) -> Option<i128> {
        if self.closes.len() < SHORT_DAYS {
            return None;
        }
        Some(
            self.closes
                .iter()
                .rev()
                .take(SHORT_DAYS)
                .map(|&close| i128::from(close))
                .sum(),
        )
    }
}

struct UptrendPullback {
    daily: DailyCloses,
}

impl Strategy for UptrendPullback {
    fn name(&self) -> String {
        "uptrend-pullback(30,90)".to_owned()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let minute_ns = tape_guest::TimeDelta::from_mins(1).as_nanos();
        let minute_of_day = (bar.close_ts.as_nanos() / minute_ns) % 1440;
        // A bar closing at UTC midnight is the final one-minute bar of the day.
        if minute_of_day != 0 {
            return;
        }

        self.daily.push(bar.close.raw());
        if !self.daily.averages_ready() || ctx.open_orders() != 0 {
            return;
        }

        let Some(short_sum) = self.daily.short_sum() else {
            return;
        };
        let long_sum = self.daily.sum;
        let short_trend_above_long = short_sum * LONG_DAYS as i128
            > long_sum * SHORT_DAYS as i128;
        let close_at_least_five_percent_below_short_average =
            i128::from(bar.close.raw()) * SHORT_DAYS as i128 * 100
                <= short_sum * 95;
        let position = ctx.position(INSTRUMENT);

        if position.is_zero() {
            if short_trend_above_long && close_at_least_five_percent_below_short_average {
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
        } else if position.is_positive() {
            let close_at_or_above_short_average =
                i128::from(bar.close.raw()) * SHORT_DAYS as i128 >= short_sum;
            if close_at_or_above_short_average || !short_trend_above_long {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
        }
    }
}

export_strategy!(UptrendPullback {
    daily: DailyCloses::new(),
});
