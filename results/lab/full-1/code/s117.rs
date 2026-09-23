use tape_guest::{Bar, Ctx, Fill, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

struct WeeklyExpiryRelease {
    expiry_reference: Option<(i64, Price)>,
    processed_day: Option<i64>,
    entry_ts_ns: Option<i64>,
}

impl Strategy for WeeklyExpiryRelease {
    fn name(&self) -> String {
        String::from("weekly-expiry-release")
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_zero() {
            self.entry_ts_ns = None;
        }

        let now_ns = bar.close_ts.as_nanos();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let day = now_ns / day_ns;
        let minute_of_day = (now_ns / minute_ns) % 1440;
        let weekday = day % 7; // 0 = Thursday; 1 = Friday.

        if weekday == 1 && minute_of_day == 480 {
            self.expiry_reference = Some((day, bar.close));
        }

        if weekday == 1 && minute_of_day == 600 && self.processed_day != Some(day) {
            self.processed_day = Some(day);

            let positive_return = self
                .expiry_reference
                .as_ref()
                .is_some_and(|(reference_day, reference_price)| {
                    *reference_day == day && bar.close > *reference_price
                });

            if positive_return
                && ctx.position(INSTRUMENT).is_zero()
                && ctx.open_orders() == 0
            {
                let qty = ctx.affordable_qty(
                    INSTRUMENT,
                    bar.close,
                    ctx.equity(),
                    SIZING_MARGIN_PPM,
                );
                if qty.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                }
            }
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() && ctx.open_orders() == 0 {
            if let Some(entry_ns) = self.entry_ts_ns {
                let holding_ns = TimeDelta::from_mins(720).as_nanos();
                if now_ns.saturating_sub(entry_ns) >= holding_ns {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                }
            }
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Buy && self.entry_ts_ns.is_none() {
            self.entry_ts_ns = Some(fill.ts.as_nanos());
        }
    }
}

export_strategy!(WeeklyExpiryRelease {
    expiry_reference: None,
    processed_day: None,
    entry_ts_ns: None,
});
