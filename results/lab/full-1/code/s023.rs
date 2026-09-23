use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

struct PaydayFlowWindows {
    last_event_serial: Option<i64>,
}

// Return the serial number of the latest payday event that has come due this
// month, along with whether that event is a buy. Days are derived from the UTC
// timestamp rather than using a fixed calendar range.
fn latest_due_event(timestamp_nanos: i64) -> (i64, bool) {
    let day_nanos = TimeDelta::from_days(1).as_nanos();
    let mut epoch_days = timestamp_nanos / day_nanos;
    if timestamp_nanos < 0 && timestamp_nanos % day_nanos != 0 {
        epoch_days -= 1;
    }

    // Convert days since the Unix epoch to a Gregorian UTC calendar date.
    let z = epoch_days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524
        - day_of_era / 146_096)
        / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era
        - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    if month <= 2 {
        year += 1;
    }

    let event_slot = if day >= 18 {
        4
    } else if day >= 15 {
        3
    } else if day >= 4 {
        2
    } else {
        1
    };
    let is_buy = event_slot == 1 || event_slot == 3;
    let month_serial = year * 12 + month - 1;
    (month_serial * 4 + event_slot, is_buy)
}

impl Strategy for PaydayFlowWindows {
    fn name(&self) -> String {
        "payday-flow-windows".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let (event_serial, is_buy) = latest_due_event(bar.close_ts.as_nanos());
        if self
            .last_event_serial
            .is_some_and(|last| event_serial <= last)
        {
            return;
        }

        // Leave the event unrecorded so it can be acted on once any existing
        // order has finished.
        if ctx.open_orders() != 0 {
            return;
        }
        self.last_event_serial = Some(event_serial);

        if is_buy {
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        } else {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
        }
    }
}

export_strategy!(PaydayFlowWindows {
    last_event_serial: None,
});
