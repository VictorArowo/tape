use tape_guest::{export_strategy, Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

struct PaydayFlowWindow {
    exit_at_ns: Option<i64>,
}

fn is_payday_midnight(ts_ns: i64) -> bool {
    let day_ns = TimeDelta::from_days(1).as_nanos();
    if ts_ns.rem_euclid(day_ns) != 0 {
        return false;
    }

    // Convert the UTC day number to a Gregorian month/day.
    let z = ts_ns.div_euclid(day_ns) + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524
        - day_of_era / 146_096)
        / 365;
    let day_of_year = day_of_era
        - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day_of_month = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };

    (month == 1 || month == 2 || month == 3 || month == 4 || month == 5 || month == 6
        || month == 7 || month == 8 || month == 9 || month == 10 || month == 11 || month == 12)
        && (day_of_month == 1 || day_of_month == 15)
}

impl Strategy for PaydayFlowWindow {
    fn name(&self) -> String {
        "payday-flow-window".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let now_ns = bar.close_ts.as_nanos();

        if let Some(exit_at_ns) = self.exit_at_ns {
            if now_ns < exit_at_ns {
                return;
            }

            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                if ctx.open_orders() == 0 {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                }
                return;
            }
            if !position.is_zero() || ctx.open_orders() != 0 {
                return;
            }
            self.exit_at_ns = None;
        }

        if !ctx.position(INSTRUMENT).is_zero() || ctx.open_orders() != 0 {
            return;
        }
        if !is_payday_midnight(now_ns) {
            return;
        }

        let Some(exit_at_ns) = now_ns.checked_add(TimeDelta::from_hours(72).as_nanos()) else {
            return;
        };
        let budget = ctx.cash().mul_ratio(997, 1_000);
        let qty = ctx.affordable_qty(INSTRUMENT, bar.close, budget, SIZING_MARGIN_PPM);
        if qty.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            self.exit_at_ns = Some(exit_at_ns);
        }
    }
}

export_strategy!(PaydayFlowWindow { exit_at_ns: None });
