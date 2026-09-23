use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const FRIDAY: i64 = 1;
const REFERENCE_MINUTE: i64 = 239; // 03:59 UTC
const ENTRY_MINUTE: i64 = 479; // 07:59 UTC
const EXIT_MINUTE: i64 = 719; // 11:59 UTC

struct WeeklyOptionsHedgeUnwind {
    reference_close: Option<(i64, f64)>,
    evaluated_day: Option<i64>,
    exit_checked_day: Option<i64>,
}

impl Strategy for WeeklyOptionsHedgeUnwind {
    fn name(&self) -> String {
        "weekly-options-hedge-unwind".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let nanos = bar.open_ts.as_nanos();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let day = nanos.div_euclid(day_ns);
        let minute_of_day = nanos.div_euclid(minute_ns).rem_euclid(1440);

        if day.rem_euclid(7) != FRIDAY {
            return;
        }

        if minute_of_day == REFERENCE_MINUTE {
            self.reference_close = Some((day, bar.close.to_f64()));
        }

        if minute_of_day == ENTRY_MINUTE && self.evaluated_day != Some(day) {
            self.evaluated_day = Some(day);

            let Some((reference_day, reference_close)) = self.reference_close else {
                return;
            };
            if reference_day != day || reference_close <= 0.0 {
                return;
            }

            let return_value = bar.close.to_f64() / reference_close - 1.0;
            if return_value <= -0.004
                && ctx.position(INSTRUMENT).is_zero()
                && ctx.open_orders() == 0
            {
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

        if minute_of_day == EXIT_MINUTE && self.exit_checked_day != Some(day) {
            self.exit_checked_day = Some(day);
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() && ctx.open_orders() == 0 {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
        }
    }
}

export_strategy!(WeeklyOptionsHedgeUnwind {
    reference_close: None,
    evaluated_day: None,
    exit_checked_day: None,
});
