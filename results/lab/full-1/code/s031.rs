use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

struct SkippedDayMomentum {
    daily_closes: VecDeque<(i64, i64)>,
    last_daily_day: Option<i64>,
    target_long: Option<bool>,
}

impl SkippedDayMomentum {
    fn update_daily_close(&mut self, bar: &Bar) -> Option<(i64, i64)> {
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let close_ns = bar.close_ts.as_nanos();
        let close_minute = close_ns.div_euclid(minute_ns).rem_euclid(1440);
        let timestamp_day = close_ns.div_euclid(day_ns);

        // Support both minute-stamped closes (23:59) and end-of-minute timestamps (00:00).
        let daily_day = match close_minute {
            1439 => timestamp_day,
            0 => timestamp_day - 1,
            _ => return None,
        };

        if self.last_daily_day == Some(daily_day) {
            return None;
        }
        self.last_daily_day = Some(daily_day);
        self.daily_closes.push_back((daily_day, bar.close.raw()));
        while self.daily_closes.len() > 16 {
            self.daily_closes.pop_front();
        }

        let close_8_days_ago = self
            .daily_closes
            .iter()
            .find(|(day, _)| *day == daily_day - 8)
            .map(|(_, price)| *price)?;
        let close_1_day_ago = self
            .daily_closes
            .iter()
            .find(|(day, _)| *day == daily_day - 1)
            .map(|(_, price)| *price)?;

        Some((close_8_days_ago, close_1_day_ago))
    }

    fn apply_signal(&mut self, ctx: &Ctx, old_close: i64, recent_close: i64) {
        let currently_long = self
            .target_long
            .unwrap_or_else(|| ctx.position(INSTRUMENT).is_positive());
        let old = i128::from(old_close);
        let recent = i128::from(recent_close);

        self.target_long = Some(if currently_long {
            // Exit below -0.5%; otherwise retain the current state.
            !(recent * 1_000 < old * 995)
        } else {
            // Enter above +0.5%; otherwise retain the current state.
            recent * 1_000 > old * 1_005
        });
    }
}

impl Strategy for SkippedDayMomentum {
    fn name(&self) -> String {
        "skipped-day-momentum".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        if let Some((old_close, recent_close)) = self.update_daily_close(bar) {
            self.apply_signal(ctx, old_close, recent_close);
        }

        if ctx.open_orders() != 0 {
            return;
        }

        let Some(target_long) = self.target_long else {
            return;
        };
        let volume_cap = bar.volume.mul_ratio(1, 10);
        if !volume_cap.is_positive() {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if target_long {
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            let qty = if affordable < volume_cap {
                affordable
            } else {
                volume_cap
            };
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        } else if position.is_positive() {
            let qty = if position < volume_cap {
                position
            } else {
                volume_cap
            };
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
            }
        }
    }
}

export_strategy!(SkippedDayMomentum {
    daily_closes: VecDeque::new(),
    last_daily_day: None,
    target_long: None,
});
