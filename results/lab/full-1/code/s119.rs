use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const LAST_MINUTE_OF_DAY: i64 = 1_439;

struct WeekendLiquidityAvoidance {
    seen_first_bar: bool,
}

impl WeekendLiquidityAvoidance {
    fn buy_with_available_cash(&self, ctx: &mut Ctx, price: Price) {
        let qty = ctx.affordable_qty(
            INSTRUMENT,
            price,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        if qty.is_positive() && ctx.open_orders() == 0 {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }
}

impl Strategy for WeekendLiquidityAvoidance {
    fn name(&self) -> String {
        "weekend-liquidity-avoidance".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        if !self.seen_first_bar {
            self.seen_first_bar = true;
            if ctx.open_orders() == 0 {
                self.buy_with_available_cash(ctx, bar.close);
            }
            return;
        }

        if ctx.open_orders() != 0 {
            return;
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let total_minutes = bar.close_ts.as_nanos().div_euclid(minute_ns);
        let minute_of_day = total_minutes.rem_euclid(1_440);
        let day_of_week = total_minutes.div_euclid(1_440).rem_euclid(7);

        if minute_of_day != LAST_MINUTE_OF_DAY {
            return;
        }

        if day_of_week == 1 {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                ctx.submit(OrderRequest::market(
                    INSTRUMENT,
                    Side::Sell,
                    position,
                ));
            }
        } else if day_of_week == 3 {
            self.buy_with_available_cash(ctx, bar.close);
        }
    }
}

export_strategy!(WeekendLiquidityAvoidance {
    seen_first_bar: false,
});
