use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const HOURLY_CLOSES_REQUIRED: usize = 7;

struct CascadeCooldownExit {
    hourly_closes: VecDeque<Price>,
    last_exit_signal_ns: Option<i64>,
}

impl Strategy for CascadeCooldownExit {
    fn name(&self) -> String {
        "cascade-cooldown-exit".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        // A bar closing on the UTC hour boundary completes that hourly close.
        let hour_ns = TimeDelta::from_hours(1).as_nanos();
        if bar.close_ts.as_nanos() % hour_ns != 0 {
            return;
        }

        self.hourly_closes.push_back(bar.close);
        if self.hourly_closes.len() > HOURLY_CLOSES_REQUIRED {
            self.hourly_closes.pop_front();
        }
        if self.hourly_closes.len() < HOURLY_CLOSES_REQUIRED {
            return;
        }

        let Some(six_hours_ago_raw) = self.hourly_closes.front().map(|p| p.raw()) else {
            return;
        };
        let Some(latest_raw) = self.hourly_closes.back().map(|p| p.raw()) else {
            return;
        };

        if ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        let sharp_decline = i128::from(latest_raw) * 1_000
            <= i128::from(six_hours_ago_raw) * 975;

        if position.is_positive() {
            if sharp_decline {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                self.last_exit_signal_ns = Some(bar.close_ts.as_nanos());
            }
            return;
        }

        if !position.is_zero() {
            return;
        }

        let cooldown_ns = TimeDelta::from_hours(12).as_nanos();
        let now_ns = bar.close_ts.as_nanos();
        let cooldown_elapsed = self
            .last_exit_signal_ns
            .map_or(true, |exit_ns| now_ns >= exit_ns.saturating_add(cooldown_ns));
        let rising = latest_raw > six_hours_ago_raw;

        if cooldown_elapsed && rising {
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

export_strategy!(CascadeCooldownExit {
    hourly_closes: VecDeque::with_capacity(HOURLY_CLOSES_REQUIRED),
    last_exit_signal_ns: None,
});
