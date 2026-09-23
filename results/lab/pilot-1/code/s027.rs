use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const LOOKBACK_DAYS: usize = 90;
const HISTORY_LEN: usize = LOOKBACK_DAYS + 1;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct WeeklyMomentumOverlay {
    daily_closes: VecDeque<i64>,
    target_invested: Option<bool>,
}

impl WeeklyMomentumOverlay {
    fn rebalance(&self, ctx: &mut Ctx, bar: &Bar) {
        if ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        let volume_cap_raw = bar.volume.raw().max(0) / 10;
        if volume_cap_raw <= 0 {
            return;
        }

        if self.target_invested == Some(true) {
            let price_raw = bar.close.raw();
            if price_raw <= 0 {
                return;
            }
            let equity_raw = ctx.equity().raw().max(0);
            let target_qty_raw = (i128::from(equity_raw) * 100_000_000_i128
                / i128::from(price_raw)) as i64;
            let shortfall_raw = target_qty_raw.saturating_sub(position.raw());
            if shortfall_raw <= 0 {
                return;
            }
            let desired = tape_guest::Qty::from_raw(shortfall_raw.min(volume_cap_raw));
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            let qty = if desired < affordable { desired } else { affordable };
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        } else if self.target_invested == Some(false) && position.is_positive() {
            let qty = tape_guest::Qty::from_raw(position.raw().min(volume_cap_raw));
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
            }
        }
    }
}

impl Strategy for WeeklyMomentumOverlay {
    fn name(&self) -> String {
        "Weekly Momentum Overlay".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let minute_ns = tape_guest::TimeDelta::from_mins(1).as_nanos();
        let day_ns = tape_guest::TimeDelta::from_days(1).as_nanos();
        if minute_ns <= 0 || day_ns <= 0 {
            return;
        }
        let minute_of_day = (bar.close_ts.as_nanos() / minute_ns) % 1440;
        let day_index = bar.close_ts.as_nanos() / day_ns;

        // A bar closing at midnight completes the preceding UTC daily candle.
        if minute_of_day == 0 {
            self.daily_closes.push_back(bar.close.raw());
            if self.daily_closes.len() > HISTORY_LEN {
                self.daily_closes.pop_front();
            }

            // 0 = Thursday, so Monday is day-of-week 4.
            if day_index % 7 == 4 && self.daily_closes.len() == HISTORY_LEN {
                let oldest = self.daily_closes.front().copied().unwrap_or(0);
                let latest = self.daily_closes.back().copied().unwrap_or(0);
                if oldest > 0 {
                    self.target_invested = Some(latest > oldest);
                }
            }
        }

        self.rebalance(ctx, bar);
    }
}

export_strategy!(WeeklyMomentumOverlay {
    daily_closes: VecDeque::with_capacity(HISTORY_LEN + 1),
    target_invested: None,
});
