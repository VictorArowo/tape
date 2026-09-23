use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const MONDAY: i64 = 4; // Weekday numbering: Thursday = 0.
const FRIDAY: i64 = 1;
const MIDNIGHT: i64 = 0;
const FRIDAY_EXIT_MINUTE: i64 = 1_438;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct WeekdayLiquidityExposure {
    entry_due: bool,
    entry_target_raw: Option<i64>,
    exit_pending: bool,
}

impl WeekdayLiquidityExposure {
    fn new() -> Self {
        Self {
            entry_due: false,
            entry_target_raw: None,
            exit_pending: false,
        }
    }
}

impl Strategy for WeekdayLiquidityExposure {
    fn name(&self) -> String {
        "weekday-liquidity-exposure".to_owned()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let timestamp_ns = bar.close_ts.as_nanos();
        let minute_of_day = (timestamp_ns / minute_ns) % 1_440;
        let day_of_week = (timestamp_ns / day_ns) % 7;

        if day_of_week == MONDAY && minute_of_day == MIDNIGHT {
            self.entry_due = true;
        }
        if day_of_week == FRIDAY && minute_of_day == FRIDAY_EXIT_MINUTE {
            self.entry_due = false;
            self.entry_target_raw = None;
            self.exit_pending = true;
        }

        // Wait for any previously submitted order to finish before submitting
        // another. IOC remainders are retried on later bars below.
        if ctx.open_orders() != 0 {
            return;
        }

        if self.exit_pending {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                return;
            }
            self.exit_pending = false;
        }

        if self.entry_due {
            let position = ctx.position(INSTRUMENT);
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            let target_raw = position.raw().saturating_add(affordable.raw());
            self.entry_target_raw = Some(target_raw);
            self.entry_due = false;
        }

        if let Some(target_raw) = self.entry_target_raw {
            let position = ctx.position(INSTRUMENT);
            let remaining_raw = target_raw.saturating_sub(position.raw());
            if remaining_raw > 0 {
                ctx.submit(OrderRequest::market(
                    INSTRUMENT,
                    Side::Buy,
                    Qty::from_raw(remaining_raw),
                ));
            } else {
                self.entry_target_raw = None;
            }
        }
    }
}

export_strategy!(WeekdayLiquidityExposure::new());
