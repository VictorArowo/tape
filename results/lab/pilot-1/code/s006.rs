use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeInForce, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const MINUTES_PER_DAY: i64 = 1_440;

struct WeekendLiquidityAvoidance {
    pending_order: Option<tape_guest::OrderId>,
    pending_side: Option<Side>,
}

impl WeekendLiquidityAvoidance {
    fn utc_day_and_minute(bar: &Bar) -> (i64, i64) {
        let minute_ns = tape_guest::TimeDelta::from_mins(1).as_nanos();
        let day_ns = tape_guest::TimeDelta::from_days(1).as_nanos();
        let nanos = bar.close_ts.as_nanos();
        (nanos / day_ns % 7, nanos / minute_ns % MINUTES_PER_DAY)
    }

    // The SDK numbers weekdays from Thursday = 0: Friday = 1, Saturday = 2,
    // Sunday = 3, and Monday = 4.
    fn btc_window(day: i64) -> bool {
        matches!(day, 0 | 4 | 5 | 6)
    }

    fn submit_buy(&mut self, ctx: &mut Ctx, bar: &Bar) {
        let qty = ctx.affordable_qty(INSTRUMENT, bar.close, ctx.cash(), SIZING_MARGIN_PPM);
        if qty.is_positive() {
            let id = ctx.submit(
                OrderRequest::market(INSTRUMENT, Side::Buy, qty)
                    .with_time_in_force(TimeInForce::Gtc),
            );
            self.pending_order = Some(id);
            self.pending_side = Some(Side::Buy);
        }
    }

    fn submit_sell(&mut self, ctx: &mut Ctx) {
        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            let id = ctx.submit(
                OrderRequest::market(INSTRUMENT, Side::Sell, position)
                    .with_time_in_force(TimeInForce::Gtc),
            );
            self.pending_order = Some(id);
            self.pending_side = Some(Side::Sell);
        }
    }
}

impl Strategy for WeekendLiquidityAvoidance {
    fn name(&self) -> String {
        "Weekend Liquidity Avoidance".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let (day, minute) = Self::utc_day_and_minute(bar);
        let weekday_btc_window = Self::btc_window(day);
        let sunday_buy_boundary = day == 3 && minute == MINUTES_PER_DAY - 1;
        let friday_sell_boundary = day == 1 && minute == MINUTES_PER_DAY - 1;
        let should_hold_btc = weekday_btc_window || sunday_buy_boundary;

        if let Some(side) = self.pending_side {
            // Let a GTC order carry any volume-capped remainder through its
            // target window, but cancel it when that window is over.
            let window_ended = match side {
                Side::Buy => !should_hold_btc,
                Side::Sell => should_hold_btc && !sunday_buy_boundary,
            };
            if window_ended {
                if let Some(id) = self.pending_order.take() {
                    ctx.cancel(id);
                }
                self.pending_side = None;
                return;
            }
            if ctx.open_orders() > 0 {
                return;
            }
            self.pending_order = None;
            self.pending_side = None;
        }

        if ctx.open_orders() > 0 {
            return;
        }

        if should_hold_btc {
            if !ctx.position(INSTRUMENT).is_positive() || weekday_btc_window {
                self.submit_buy(ctx, bar);
            }
        } else if !ctx.position(INSTRUMENT).is_zero() {
            self.submit_sell(ctx);
        } else if friday_sell_boundary {
            self.submit_sell(ctx);
        }
    }
}

export_strategy!(WeekendLiquidityAvoidance {
    pending_order: None,
    pending_side: None,
});
