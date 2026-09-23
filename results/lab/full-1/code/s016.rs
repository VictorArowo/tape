use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, TimeInForce,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Waiting,
    Buying,
    Holding,
    Selling,
}

struct WeekdayLiquidityWindow {
    phase: Phase,
    target_qty: Qty,
}

impl Strategy for WeekdayLiquidityWindow {
    fn name(&self) -> String {
        "weekday-liquidity-window".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        // A bar closing at midnight is followed by the bar opening at that UTC boundary.
        let ts = bar.close_ts.as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_of_week = ts.div_euclid(day_ns).rem_euclid(7);
        let minute_of_day = ts.rem_euclid(day_ns) / minute_ns;
        let monday_midnight = day_of_week == 4 && minute_of_day == 0;
        let saturday_midnight = day_of_week == 2 && minute_of_day == 0;

        let position = ctx.position(INSTRUMENT);

        if saturday_midnight {
            self.phase = Phase::Selling;
            self.target_qty = Qty::from_raw(0);
        }

        if monday_midnight && position.is_zero() && self.phase != Phase::Buying {
            self.phase = Phase::Buying;
            self.target_qty = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
        }

        match self.phase {
            Phase::Waiting => {
                if position.is_positive() {
                    self.phase = Phase::Holding;
                }
            }
            Phase::Buying => {
                if position.raw() >= self.target_qty.raw() {
                    self.phase = Phase::Holding;
                } else if ctx.open_orders() == 0 {
                    let remaining = self.target_qty.raw().saturating_sub(position.raw());
                    if remaining > 0 {
                        let affordable = ctx.affordable_qty(
                            INSTRUMENT,
                            bar.close,
                            ctx.cash(),
                            SIZING_MARGIN_PPM,
                        );
                        let qty_raw = remaining.min(affordable.raw());
                        if qty_raw > 0 && ctx.open_orders() == 0 {
                            let _ = ctx.submit(
                                OrderRequest::market(
                                    INSTRUMENT,
                                    Side::Buy,
                                    Qty::from_raw(qty_raw),
                                )
                                .with_time_in_force(TimeInForce::Ioc),
                            );
                        }
                    }
                }
            }
            Phase::Holding => {
                if saturday_midnight {
                    self.phase = Phase::Selling;
                }
            }
            Phase::Selling => {
                if position.is_zero() {
                    self.phase = Phase::Waiting;
                    self.target_qty = Qty::from_raw(0);
                } else if ctx.open_orders() == 0 && position.is_positive() {
                    let _ = ctx.submit(
                        OrderRequest::market(INSTRUMENT, Side::Sell, position)
                            .with_time_in_force(TimeInForce::Ioc),
                    );
                }
            }
        }
    }
}

export_strategy!(WeekdayLiquidityWindow {
    phase: Phase::Waiting,
    target_qty: Qty::from_raw(0),
});
