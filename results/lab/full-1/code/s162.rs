use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, TimeInForce,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

struct WeeklyExpiryUnwind {
    seven_am_close: Option<(i64, Price)>,
    attempted_friday: Option<i64>,
    holding_friday: Option<i64>,
}

impl Strategy for WeeklyExpiryUnwind {
    fn name(&self) -> String {
        "weekly-expiry-unwind".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        // Identify candles by their UTC opening time. The 08:59 candle closes
        // at 09:00, when an order can be sent for the next candle's open.
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let open_ns = bar.open_ts.as_nanos();
        let day = open_ns.div_euclid(day_ns);
        let minute_of_day = open_ns.rem_euclid(day_ns) / minute_ns;
        let day_of_week = day.rem_euclid(7); // 0 = Thursday; Friday = 1.

        if minute_of_day == 420 {
            self.seven_am_close = Some((day, bar.close));
        }

        // The bar opening at 16:59 closes at 17:00. Submit now for the
        // 17:00 open, eight hours after the 09:00 entry open.
        if day_of_week == 1
            && minute_of_day == 1_019
            && self.holding_friday == Some(day)
        {
            self.holding_friday = None;
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() && ctx.open_orders() == 0 {
                ctx.submit(
                    OrderRequest::market(INSTRUMENT, Side::Sell, position)
                        .with_time_in_force(TimeInForce::Ioc),
                );
            }
            return;
        }

        // The candle opening at 08:59 is the final candle of the 08:00–08:59
        // hour; its close is compared with the 07:00 candle's close.
        if day_of_week != 1
            || minute_of_day != 539
            || self.attempted_friday == Some(day)
        {
            return;
        }

        let Some((reference_day, reference_close)) = self.seven_am_close else {
            return;
        };
        if reference_day != day
            || !reference_close.is_positive()
            || bar.close <= reference_close
            || !ctx.position(INSTRUMENT).is_zero()
        {
            return;
        }

        // This Friday's signal is considered once, even if it cannot be
        // traded because there is an outstanding order or insufficient cash.
        self.attempted_friday = Some(day);
        if ctx.open_orders() != 0 {
            return;
        }

        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        let volume_cap = bar.volume.mul_ratio(1, 10);
        let qty = if affordable < volume_cap {
            affordable
        } else {
            volume_cap
        };

        if qty.is_positive() {
            self.holding_friday = Some(day);
            ctx.submit(
                OrderRequest::market(INSTRUMENT, Side::Buy, qty)
                    .with_time_in_force(TimeInForce::Ioc),
            );
        }
    }
}

export_strategy!(WeeklyExpiryUnwind {
    seven_am_close: None,
    attempted_friday: None,
    holding_friday: None,
});
