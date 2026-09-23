use tape_guest::{
    export_strategy, Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy,
    TimeDelta,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

struct CmeReopenContinuation {
    first_hour_open: Option<(i64, i64)>, // UTC day number, raw price
    last_signal_day: Option<i64>,
    entry_ts_nanos: Option<i64>,
}

impl CmeReopenContinuation {
    fn new() -> Self {
        Self {
            first_hour_open: None,
            last_signal_day: None,
            entry_ts_nanos: None,
        }
    }

    fn signal_hour_utc(day: i64) -> i64 {
        if is_us_daylight_time(day) { 22 } else { 23 }
    }
}

impl Strategy for CmeReopenContinuation {
    fn name(&self) -> String {
        "CME Reopen Continuation".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        // An order submitted when this bar closes can execute at the next bar's open.
        if let Some(entry_ts) = self.entry_ts_nanos {
            let exit_ts = entry_ts.checked_add(TimeDelta::from_hours(6).as_nanos());
            if let Some(exit_ts) = exit_ts {
                if bar.close_ts.as_nanos() >= exit_ts {
                    let position = ctx.position(INSTRUMENT);
                    if position.is_positive() && ctx.open_orders() == 0 {
                        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                    }
                }
            }
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let open_minute = bar.open_ts.as_nanos().div_euclid(minute_ns);
        let day = open_minute.div_euclid(1440);
        let minute_of_day = open_minute.rem_euclid(1440);

        // UTC day numbering starts on Thursday, so Sunday is day-of-week 3.
        if day.rem_euclid(7) != 3 {
            return;
        }

        let hour = Self::signal_hour_utc(day);
        let hour_start = hour * 60;
        let first_minute = hour_start;
        let last_minute = (hour_start + 59).rem_euclid(1440);

        if minute_of_day == first_minute && self.last_signal_day != Some(day) {
            self.first_hour_open = Some((day, bar.open.raw()));
        }

        if minute_of_day != last_minute || self.last_signal_day == Some(day) {
            return;
        }

        // Consume this Sunday's signal even if it is weak, blocked, or the account is not flat.
        self.last_signal_day = Some(day);
        let Some((signal_day, hour_open_raw)) = self.first_hour_open.take() else {
            return;
        };
        if signal_day != day || hour_open_raw <= 0 {
            return;
        }

        // Compare the return to +0.30% without floating-point rounding.
        let strong_up_move = i128::from(bar.close.raw()) * 1_000
            >= i128::from(hour_open_raw) * 1_003;
        if !strong_up_move || !ctx.position(INSTRUMENT).is_zero() || ctx.open_orders() != 0 {
            return;
        }

        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        // Only use volume already observed at the signal bar; do not look ahead to the next bar.
        let volume_cap = bar.volume.mul_ratio(10, 100);
        let qty = Qty::from_raw(affordable.raw().min(volume_cap.raw()));
        if qty.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.side == Side::Buy {
            if self.entry_ts_nanos.is_none() {
                self.entry_ts_nanos = Some(fill.ts.as_nanos());
            }
        } else if fill.side == Side::Sell && ctx.position(INSTRUMENT).is_zero() {
            self.entry_ts_nanos = None;
        }
    }
}

fn is_us_daylight_time(day: i64) -> bool {
    let (year, month, day_of_month) = civil_from_days(day);

    if month < 3 || month > 11 {
        return false;
    }
    if month > 3 && month < 11 {
        return true;
    }

    if month == 3 {
        let first_day = days_from_civil(year, 3, 1);
        let first_sunday = 1 + (3 - first_day.rem_euclid(7)).rem_euclid(7);
        return day_of_month >= first_sunday + 7;
    }

    let first_day = days_from_civil(year, 11, 1);
    let first_sunday = 1 + (3 - first_day.rem_euclid(7)).rem_euclid(7);
    day_of_month < first_sunday
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted / 146_097
    } else {
        (shifted - 146_096) / 146_097
    };
    let day_of_era = shifted - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524
        - day_of_era / 146_096)
        / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era
        - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let adjusted_year = year - i64::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year / 400
    } else {
        (adjusted_year - 399) / 400
    };
    let year_of_era = adjusted_year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

export_strategy!(CmeReopenContinuation::new());