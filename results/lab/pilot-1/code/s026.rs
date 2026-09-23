use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Idle,
    BuyWindow,
    SellUntilFlat,
}

struct EarlyMonthFlowWindow {
    mode: Mode,
}

// Convert a Unix day number to its Gregorian day-of-month using calendar
// arithmetic, without relying on a fixed date range.
fn day_of_month(unix_days: i64) -> i64 {
    let z = unix_days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    doy - (153 * mp + 2) / 5 + 1
}

impl Strategy for EarlyMonthFlowWindow {
    fn name(&self) -> String {
        "early-month-flow-window".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let timestamp = bar.close_ts.as_nanos();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let minute_of_day = (timestamp / minute_ns).rem_euclid(1_440);
        let day = day_of_month(timestamp.div_euclid(day_ns));

        // Calendar-window state changes occur on the bar closing at 00:00 UTC.
        if minute_of_day == 0 {
            if day == 2 {
                self.mode = Mode::BuyWindow;
            } else if day == 8 {
                self.mode = Mode::SellUntilFlat;
            }
        }

        if ctx.open_orders() != 0 {
            return;
        }

        match self.mode {
            Mode::Idle => {}
            Mode::BuyWindow => {
                // Stop buying at the start of day 8. If the buy was only
                // partially filled, cash-based sizing retries with available cash.
                if day >= 8 {
                    self.mode = Mode::SellUntilFlat;
                    let position = ctx.position(INSTRUMENT);
                    if position.is_positive() {
                        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                    }
                    return;
                }

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
            Mode::SellUntilFlat => {
                let position = ctx.position(INSTRUMENT);
                if position.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                } else {
                    self.mode = Mode::Idle;
                }
            }
        }
    }
}

export_strategy!(EarlyMonthFlowWindow { mode: Mode::Idle });
