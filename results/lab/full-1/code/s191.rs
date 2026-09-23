use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const HISTORY_DAYS: usize = 6;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct FiveDayMomentumHysteresis {
    daily_closes: VecDeque<i64>,
}

impl Strategy for FiveDayMomentumHysteresis {
    fn name(&self) -> String {
        "five-day-momentum-hysteresis".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        // A bar closing at 00:00 UTC completes the previous UTC day's data.
        let day_ns = TimeDelta::from_days(1).as_nanos();
        if day_ns <= 0 || bar.close_ts.as_nanos().rem_euclid(day_ns) != 0 {
            return;
        }

        self.daily_closes.push_back(bar.close.raw());
        if self.daily_closes.len() > HISTORY_DAYS {
            self.daily_closes.pop_front();
        }
        if self.daily_closes.len() < HISTORY_DAYS || ctx.open_orders() != 0 {
            return;
        }

        let Some(&five_days_ago) = self.daily_closes.front() else {
            return;
        };
        let Some(&today) = self.daily_closes.back() else {
            return;
        };
        if five_days_ago <= 0 {
            return;
        }

        let current_scaled = i128::from(today) * 100;
        let reference_scaled = i128::from(five_days_ago);
        let position = ctx.position(INSTRUMENT);

        if position.is_zero() && current_scaled > reference_scaled * 102 {
            // Size using cash to avoid leverage; affordability accounts for costs.
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                // Submitted at the daily close so the market order can execute
                // on the next minute bar under the sandbox's latency settings.
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        } else if position.is_positive() && current_scaled <= reference_scaled * 99 {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
        }
    }
}

export_strategy!(FiveDayMomentumHysteresis {
    daily_closes: VecDeque::with_capacity(HISTORY_DAYS),
});
