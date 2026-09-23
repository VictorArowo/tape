use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const MAX_BAR_VOLUME_NUM: i64 = 1;
const MAX_BAR_VOLUME_DEN: i64 = 10;

struct WeekendExposureAvoidance;

impl Strategy for WeekendExposureAvoidance {
    fn name(&self) -> String {
        "weekend-exposure-avoidance".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT || ctx.open_orders() != 0 {
            return;
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let timestamp_ns = bar.close_ts.as_nanos();
        let minute_of_day = (timestamp_ns / minute_ns).rem_euclid(1440);
        // The SDK numbers weekdays from Thursday = 0, so Friday = 1 and Sunday = 3.
        let day_of_week = (timestamp_ns / day_ns).rem_euclid(7);

        // Exit after Friday's 23:59 bar; re-enter after Sunday's 23:59 bar.
        let should_hold_btc = match day_of_week {
            1 => minute_of_day < 1439,
            2 => false,
            3 => minute_of_day >= 1439,
            _ => true,
        };

        let volume_cap = bar.volume.mul_ratio(MAX_BAR_VOLUME_NUM, MAX_BAR_VOLUME_DEN);
        let position = ctx.position(INSTRUMENT);

        if should_hold_btc {
            if position.is_zero() {
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

export_strategy!(WeekendExposureAvoidance);