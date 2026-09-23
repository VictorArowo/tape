use tape_guest::{
    Bar, Ctx, Fill, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const FRIDAY: i64 = 1; // UTC weekday numbering: Thursday = 0.
const PRE_EXPIRY_START_MINUTE: i64 = 2 * 60;
const EXPIRY_MINUTE: i64 = 8 * 60;
const SIGNAL_CLOSE_MINUTE: i64 = 8 * 60 + 30;

fn utc_day(timestamp_ns: i64) -> i64 {
    timestamp_ns.div_euclid(TimeDelta::from_days(1).as_nanos())
}

fn utc_minute_of_day(timestamp_ns: i64) -> i64 {
    timestamp_ns
        .div_euclid(TimeDelta::from_mins(1).as_nanos())
        .rem_euclid(24 * 60)
}

fn is_friday(day: i64) -> bool {
    day.rem_euclid(7) == FRIDAY
}

struct PostExpiryStrength {
    friday_day: Option<i64>,
    range_high_raw: Option<i64>,
    range_low_raw: Option<i64>,
    close_at_0800_raw: Option<i64>,
    signal_checked: bool,
    entry_ts_ns: Option<i64>,
    tracked_position_raw: i64,
    completed_round_trips: u64,
}

impl PostExpiryStrength {
    fn new() -> Self {
        Self {
            friday_day: None,
            range_high_raw: None,
            range_low_raw: None,
            close_at_0800_raw: None,
            signal_checked: false,
            entry_ts_ns: None,
            tracked_position_raw: 0,
            completed_round_trips: 0,
        }
    }

    fn reset_friday(&mut self, day: i64) {
        self.friday_day = Some(day);
        self.range_high_raw = None;
        self.range_low_raw = None;
        self.close_at_0800_raw = None;
        self.signal_checked = false;
    }

    fn submit_exit_if_needed(&self, ctx: &mut Ctx, bar: &Bar) -> bool {
        let position = ctx.position(INSTRUMENT);
        let Some(entry_ts_ns) = self.entry_ts_ns else {
            return false;
        };
        if !position.is_positive() {
            return false;
        }

        let exit_due = bar.close_ts.as_nanos()
            >= entry_ts_ns.saturating_add(TimeDelta::from_hours(6).as_nanos());

        // Flatten any position opened before this Friday before the Friday signal.
        let close_day = utc_day(bar.close_ts.as_nanos());
        let before_friday_signal = is_friday(close_day)
            && utc_minute_of_day(bar.close_ts.as_nanos()) >= EXPIRY_MINUTE
            && utc_day(entry_ts_ns) < close_day;

        if (exit_due || before_friday_signal) && ctx.open_orders() == 0 {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            return true;
        }
        false
    }
}

impl Strategy for PostExpiryStrength {
    fn name(&self) -> String {
        "post-expiry-strength".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let open_ts_ns = bar.open_ts.as_nanos();
        let close_ts_ns = bar.close_ts.as_nanos();
        let open_day = utc_day(open_ts_ns);
        let close_day = utc_day(close_ts_ns);

        if is_friday(open_day) && self.friday_day != Some(open_day) {
            self.reset_friday(open_day);
        }

        // Build the Friday 02:00–07:59 UTC high-low range from minute bars.
        if is_friday(open_day) {
            let open_minute = utc_minute_of_day(open_ts_ns);
            if (PRE_EXPIRY_START_MINUTE..EXPIRY_MINUTE).contains(&open_minute) {
                self.range_high_raw = Some(
                    self.range_high_raw
                        .map_or(bar.high.raw(), |high| high.max(bar.high.raw())),
                );
                self.range_low_raw = Some(
                    self.range_low_raw
                        .map_or(bar.low.raw(), |low| low.min(bar.low.raw())),
                );
            }
        }

        // The close at 08:00 is the close of the minute ending at 08:00 UTC.
        if is_friday(close_day)
            && self.friday_day == Some(close_day)
            && utc_minute_of_day(close_ts_ns) == EXPIRY_MINUTE
        {
            self.close_at_0800_raw = Some(bar.close.raw());
        }

        // Submit at the 08:30 close so a market order can execute at the next bar open.
        if is_friday(close_day)
            && self.friday_day == Some(close_day)
            && utc_minute_of_day(close_ts_ns) == SIGNAL_CLOSE_MINUTE
            && !self.signal_checked
        {
            self.signal_checked = true;
            let signal_is_strong = match (
                self.range_high_raw,
                self.range_low_raw,
                self.close_at_0800_raw,
            ) {
                (Some(high), Some(low), Some(close_0800)) => {
                    let midpoint_raw = (i128::from(high) + i128::from(low)) / 2;
                    i128::from(bar.close.raw()) > midpoint_raw
                        && bar.close.raw() > close_0800
                }
                _ => false,
            };

            if signal_is_strong
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

        self.submit_exit_if_needed(ctx, bar);
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Buy {
            if self.entry_ts_ns.is_none() {
                self.entry_ts_ns = Some(fill.ts.as_nanos());
            }
            self.tracked_position_raw = self
                .tracked_position_raw
                .saturating_add(fill.qty.raw());
        } else {
            self.tracked_position_raw = self
                .tracked_position_raw
                .saturating_sub(fill.qty.raw())
                .max(0);
            if self.tracked_position_raw == 0 && self.entry_ts_ns.take().is_some() {
                self.completed_round_trips = self.completed_round_trips.saturating_add(1);
            }
        }
    }

    fn on_finish(&mut self, ctx: &mut Ctx) {
        ctx.log(&format!(
            "post-expiry-strength completed round trips: {}",
            self.completed_round_trips
        ));
    }
}

export_strategy!(PostExpiryStrength::new());
