use tape_guest::{
    Bar, Ctx, InstrumentId, Money, OrderRequest, Price, Side, Strategy, TimeDelta, Timestamp,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const MINUTE_NANOS: i64 = TimeDelta::from_mins(1).as_nanos();
const DAY_MINUTES: i64 = 1_440;
const SIZING_MARGIN_PPM: i64 = 5_000;

const SIGNAL_MINUTE: i64 = 7 * 60;
const EXIT_MINUTE: i64 = 13 * 60;

fn utc_day_minute(ts: &Timestamp) -> (i64, i64) {
    let minute_index = ts.as_nanos().div_euclid(MINUTE_NANOS);
    (
        minute_index.div_euclid(DAY_MINUTES),
        minute_index.rem_euclid(DAY_MINUTES),
    )
}

struct EuropeHandoffBounce {
    midnight_day: Option<i64>,
    midnight_open: Option<Price>,
    last_entry_day: Option<i64>,
    entry_price: Option<Price>,
}

impl Strategy for EuropeHandoffBounce {
    fn name(&self) -> String {
        "Europe Handoff Bounce".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let (open_day, open_minute) = utc_day_minute(&bar.open_ts);
        if open_minute == 0 {
            self.midnight_day = Some(open_day);
            self.midnight_open = bar.open.is_positive().then_some(bar.open);
        }

        let (day, close_minute) = utc_day_minute(&bar.close_ts);
        let position = ctx.position(INSTRUMENT);

        // The callback follows a completed bar; a submitted market order executes
        // at the next bar's open. The host applies fill fees and slippage.
        if position.is_positive() {
            let threshold_exit = self.entry_price.is_some_and(|fill_price| {
                let close = i128::from(bar.close.raw());
                let fill = i128::from(fill_price.raw());
                close * 10_000 >= fill * 10_060 || close * 10_000 <= fill * 9_900
            });
            let time_exit = close_minute == EXIT_MINUTE;

            if threshold_exit || time_exit {
                if ctx.open_orders() == 0 {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                }
                return;
            }
        }

        if close_minute != SIGNAL_MINUTE
            || self.last_entry_day == Some(day)
            || self.midnight_day != Some(day)
            || !position.is_zero()
            || ctx.open_orders() != 0
        {
            return;
        }

        let Some(midnight_open) = self.midnight_open else {
            return;
        };
        if !midnight_open.is_positive() || !bar.close.is_positive() || !bar.volume.is_positive() {
            return;
        }

        // At least 0.75% below the 00:00 open.
        if i128::from(bar.close.raw()) * 10_000
            > i128::from(midnight_open.raw()) * 9_925
        {
            return;
        }

        // Use all equity that is also available as cash; affordable_qty accounts
        // for fees. Cap participation using the latest completed bar's volume.
        let budget_raw = ctx.equity().raw().min(ctx.cash().raw());
        if budget_raw <= 0 {
            return;
        }
        let budget = Money::from_raw(budget_raw);
        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            budget,
            SIZING_MARGIN_PPM,
        );
        let volume_cap = bar.volume.mul_ratio(1, 10);
        let qty = if affordable < volume_cap {
            affordable
        } else {
            volume_cap
        };

        if qty.is_positive() {
            self.last_entry_day = Some(day);
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.side == Side::Buy {
            self.entry_price = Some(fill.price);
        } else if fill.side == Side::Sell && ctx.position(INSTRUMENT).is_zero() {
            self.entry_price = None;
        }
    }
}

export_strategy!(EuropeHandoffBounce {
    midnight_day: None,
    midnight_open: None,
    last_entry_day: None,
    entry_price: None,
});
