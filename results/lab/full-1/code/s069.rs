use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

// Returns the Gregorian month and day for a UTC day offset from the Unix epoch.
fn month_day(days_since_epoch: i64) -> (i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 {
        z / 146_097
    } else {
        (z - 146_096) / 146_097
    };
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year =
        day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_part = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = month_part + if month_part < 10 { 3 } else { -9 };
    (month, day)
}

struct MonthEndRebalanceBid {
    prior_month_end_close: Option<Price>,
    exit_at_nanos: Option<i64>,
}

impl Strategy for MonthEndRebalanceBid {
    fn name(&self) -> String {
        "month-end-rebalance-bid".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let day_ns = TimeDelta::from_days(1).as_nanos();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let ts_nanos = bar.close_ts.as_nanos();
        let day_index = ts_nanos.div_euclid(day_ns);
        let minute_of_day = ts_nanos.rem_euclid(day_ns) / minute_ns;
        let (month, day_of_month) = month_day(day_index);

        // The bar closing at midnight on the first is the prior month's final close.
        if minute_of_day == 0 && day_of_month == 1 {
            self.prior_month_end_close = Some(bar.close);
        }

        // Submit at the open 48 hours after the month rollover. The signal occurs
        // at midnight on the final day, one day before that rollover.
        if let Some(exit_at) = self.exit_at_nanos {
            if ts_nanos >= exit_at {
                let position = ctx.position(INSTRUMENT);
                if !position.is_positive() {
                    self.exit_at_nanos = None;
                } else {
                    if ctx.open_orders() == 0 {
                        ctx.submit(OrderRequest::market(
                            INSTRUMENT,
                            Side::Sell,
                            position,
                        ));
                    }
                    return;
                }
            }
        }

        if minute_of_day != 0 {
            return;
        }
        let (next_month, _) = month_day(day_index + 1);
        if next_month == month || day_of_month == 0 {
            return;
        }

        let Some(reference_close) = self.prior_month_end_close else {
            return;
        };
        if !reference_close.is_positive() || !bar.close.is_positive() {
            return;
        }

        let month_to_date_return = bar.close.to_f64() / reference_close.to_f64() - 1.0;
        if month_to_date_return >= 0.0
            || !ctx.position(INSTRUMENT).is_zero()
            || ctx.open_orders() != 0
        {
            return;
        }

        let qty = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        if qty.is_positive() {
            let month_boundary = ts_nanos + day_ns;
            self.exit_at_nanos = Some(month_boundary + TimeDelta::from_hours(48).as_nanos());
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }
}

export_strategy!(MonthEndRebalanceBid {
    prior_month_end_close: None,
    exit_at_nanos: None,
});
