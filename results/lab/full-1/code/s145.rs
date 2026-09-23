use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta, TimeInForce,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const DAILY_CLOSE_HISTORY: usize = 32;

struct SkipWindowMomentumGate {
    current_day: Option<i64>,
    current_close: Option<Price>,
    current_volume: Qty,
    last_finalized_day: Option<i64>,
    daily_closes: VecDeque<(i64, i64)>,
}

impl SkipWindowMomentumGate {
    fn finalize_day(&mut self, ctx: &mut Ctx, day: i64, close: Price, volume: Qty) {
        if self.last_finalized_day == Some(day) {
            return;
        }
        self.last_finalized_day = Some(day);
        self.daily_closes.push_back((day, close.raw()));
        while self.daily_closes.len() > DAILY_CLOSE_HISTORY {
            self.daily_closes.pop_front();
        }

        let close_d2 = self
            .daily_closes
            .iter()
            .find(|entry| entry.0 == day.saturating_sub(2))
            .map(|entry| entry.1);
        let close_d12 = self
            .daily_closes
            .iter()
            .find(|entry| entry.0 == day.saturating_sub(12))
            .map(|entry| entry.1);
        let (Some(end), Some(start)) = (close_d2, close_d12) else {
            return;
        };
        if start <= 0 || ctx.open_orders() != 0 {
            return;
        }

        let end_scaled = i128::from(end) * 100;
        let start_scaled = i128::from(start);
        let position = ctx.position(INSTRUMENT);
        let volume_cap = volume.mul_ratio(1, 10);
        if !volume_cap.is_positive() {
            return;
        }

        if end_scaled > start_scaled * 102 && position.is_zero() {
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            let qty = if affordable > volume_cap {
                volume_cap
            } else {
                affordable
            };
            if qty.is_positive() {
                ctx.submit(
                    OrderRequest::market(INSTRUMENT, Side::Buy, qty)
                        .with_time_in_force(TimeInForce::Ioc),
                );
            }
        } else if end_scaled < start_scaled * 99 && position.is_positive() {
            let qty = if position > volume_cap {
                volume_cap
            } else {
                position
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

impl Strategy for SkipWindowMomentumGate {
    fn name(&self) -> String {
        "skip-window-momentum-gate".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let day_ns = TimeDelta::from_days(1).as_nanos();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let ts = bar.close_ts.as_nanos();
        let day = ts / day_ns;
        let minute_of_day = (ts / minute_ns) % 1440;

        if let Some(previous_day) = self.current_day {
            if previous_day != day {
                if self.last_finalized_day != Some(previous_day) {
                    if let Some(previous_close) = self.current_close {
                        self.finalize_day(
                            ctx,
                            previous_day,
                            previous_close,
                            self.current_volume,
                        );
                    }
                }
                self.current_day = Some(day);
                self.current_close = Some(bar.close);
                self.current_volume = bar.volume;
            } else {
                self.current_close = Some(bar.close);
                self.current_volume = bar.volume;
            }
        } else {
            self.current_day = Some(day);
            self.current_close = Some(bar.close);
            self.current_volume = bar.volume;
        }

        // A minute timestamp of 23:59 marks the UTC daily close; the market
        // order submitted here is eligible at the following bar's open.
        if minute_of_day == 1439 {
            self.finalize_day(ctx, day, bar.close, bar.volume);
        }
    }
}

export_strategy!(SkipWindowMomentumGate {
    current_day: None,
    current_close: None,
    current_volume: Qty::from_raw(0),
    last_finalized_day: None,
    daily_closes: VecDeque::new(),
});