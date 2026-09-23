use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, Timestamp,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const PRICE_SCALE: i128 = 100_000_000;

struct CmeReopeningGapRecovery {
    reference: Option<Price>,
    reference_week: Option<i64>,
    session_sunday: Option<i64>,
    session_reference: Option<Price>,
    session_low: Option<Price>,
    session_eligible: bool,
    session_entry_used: bool,
    entry_qty_raw: i128,
    entry_cost_raw: i128,
    entry_ts: Option<Timestamp>,
    trade_reference: Option<Price>,
}

impl CmeReopeningGapRecovery {
    fn new() -> Self {
        Self {
            reference: None,
            reference_week: None,
            session_sunday: None,
            session_reference: None,
            session_low: None,
            session_eligible: false,
            session_entry_used: false,
            entry_qty_raw: 0,
            entry_cost_raw: 0,
            entry_ts: None,
            trade_reference: None,
        }
    }

    fn average_entry_price(&self) -> Option<Price> {
        if self.entry_qty_raw <= 0 {
            return None;
        }
        let raw = self.entry_cost_raw * PRICE_SCALE / self.entry_qty_raw;
        Some(Price::from_raw(raw as i64))
    }

    fn reference_for_week(&self, week: i64) -> Option<Price> {
        if self.reference_week == Some(week) {
            self.reference
        } else {
            None
        }
    }
}

impl Strategy for CmeReopeningGapRecovery {
    fn name(&self) -> String {
        "CME Reopening Gap Recovery".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let ts_ns = bar.close_ts.as_nanos();
        let utc_day = ts_ns.div_euclid(day_ns);
        let utc_minute = ts_ns.rem_euclid(day_ns) / minute_ns;
        let weekday = utc_day.rem_euclid(7); // Thursday = 0, Friday = 1.

        // CME's Friday halt is 16:00 Chicago time. The close timestamp at
        // that boundary is the close of the final one-minute bar before halt.
        let dst_at_bar = us_dst_at(ts_ns, minute_ns, day_ns);
        let friday_halt_utc_minute = if dst_at_bar { 21 * 60 } else { 22 * 60 };
        if weekday == 1 && utc_minute == friday_halt_utc_minute {
            self.reference = Some(bar.close);
            self.reference_week = Some(utc_day.div_euclid(7));
        }

        // Manage an existing position before considering a new weekly entry.
        if ctx.position(INSTRUMENT).is_zero() {
            self.entry_qty_raw = 0;
            self.entry_cost_raw = 0;
            self.entry_ts = None;
            self.trade_reference = None;
        } else if ctx.open_orders() == 0 {
            let hit_reference = self
                .trade_reference
                .is_some_and(|reference| bar.close >= reference);
            let hit_loss_limit = self
                .average_entry_price()
                .is_some_and(|average| bar.close <= average.mul_ratio(99, 100));
            let hit_time_limit = self.entry_ts.is_some_and(|entry_ts| {
                ts_ns - entry_ts.as_nanos() >= TimeDelta::from_hours(24).as_nanos()
            });

            if hit_reference || hit_loss_limit || hit_time_limit {
                let position = ctx.position(INSTRUMENT);
                if position.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                }
            }
        }

        let Some((sunday_day, elapsed_ns)) =
            reopening_window(ts_ns, minute_ns, day_ns)
        else {
            return;
        };

        if self.session_sunday != Some(sunday_day) {
            self.session_sunday = Some(sunday_day);
            self.session_reference = self.reference_for_week(sunday_day.div_euclid(7));
            self.session_low = None;
            self.session_eligible = false;
            self.session_entry_used = false;
        }

        self.session_low = Some(match self.session_low {
            Some(low) if low <= bar.low => low,
            _ => bar.low,
        });

        let check_ns = TimeDelta::from_mins(30).as_nanos();
        let entry_window_end_ns = check_ns + TimeDelta::from_hours(6).as_nanos();
        if elapsed_ns == check_ns {
            self.session_eligible = self
                .session_reference
                .is_some_and(|reference| bar.close <= reference.mul_ratio(9_925, 10_000));
            return;
        }

        if elapsed_ns <= check_ns || elapsed_ns > entry_window_end_ns {
            return;
        }
        if !self.session_eligible
            || self.session_entry_used
            || !ctx.position(INSTRUMENT).is_zero()
            || ctx.open_orders() != 0
        {
            return;
        }

        let (Some(reference), Some(running_low)) =
            (self.session_reference, self.session_low)
        else {
            return;
        };
        let recovered = bar.close >= running_low.mul_ratio(10_030, 10_000);
        let still_below_reference = bar.close <= reference.mul_ratio(9_950, 10_000);
        if !recovered || !still_below_reference {
            return;
        }

        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        // The SDK provides no future-bar volume or participation-rate order
        // field, so constrain the submitted quantity using the latest completed
        // bar's volume. IOC fills, including partial fills, are left in place.
        let volume_cap = bar.volume.mul_ratio(1, 10);
        let qty = if affordable < volume_cap {
            affordable
        } else {
            volume_cap
        };
        if qty.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            self.session_entry_used = true;
            self.trade_reference = Some(reference);
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.side == Side::Buy {
            if self.entry_qty_raw == 0 {
                self.entry_ts = Some(fill.ts);
            }
            self.entry_qty_raw += i128::from(fill.qty.raw());
            self.entry_cost_raw += i128::from((fill.price * fill.qty).raw());
        }
    }
}

fn reopening_window(ts_ns: i64, minute_ns: i64, day_ns: i64) -> Option<(i64, i64)> {
    let utc_day = ts_ns.div_euclid(day_ns);
    let weekday = utc_day.rem_euclid(7);
    let sunday_day = match weekday {
        3 => utc_day,
        4 => utc_day - 1,
        _ => return None,
    };

    // Sunday 17:00 Chicago is 22:00 UTC during daylight time and 23:00 UTC
    // otherwise. Noon UTC gives the correct seasonal offset on that Sunday.
    let noon_utc = sunday_day * day_ns + 12 * 60 * minute_ns;
    let dst = us_dst_at(noon_utc, minute_ns, day_ns);
    let reopen_minute = if dst { 22 * 60 } else { 23 * 60 };
    let reopen_ns = sunday_day * day_ns + reopen_minute * minute_ns;
    let elapsed_ns = ts_ns - reopen_ns;
    let latest_ns = TimeDelta::from_mins(30).as_nanos() + TimeDelta::from_hours(6).as_nanos();

    if elapsed_ns > 0 && elapsed_ns <= latest_ns {
        Some((sunday_day, elapsed_ns))
    } else {
        None
    }
}

fn us_dst_at(ts_ns: i64, minute_ns: i64, day_ns: i64) -> bool {
    let day = ts_ns.div_euclid(day_ns);
    let minute = ts_ns.rem_euclid(day_ns) / minute_ns;
    let (year, month, day_of_month) = civil_from_days(day);

    if month < 3 || month > 11 {
        return false;
    }
    if month > 3 && month < 11 {
        return true;
    }

    if month == 3 {
        let first_day = days_from_civil(year, 3, 1);
        let first_weekday_sunday_zero = (first_day + 4).rem_euclid(7);
        let second_sunday = 8 + (7 - first_weekday_sunday_zero) % 7;
        day_of_month > second_sunday || (day_of_month == second_sunday && minute >= 8 * 60)
    } else {
        let first_day = days_from_civil(year, 11, 1);
        let first_weekday_sunday_zero = (first_day + 4).rem_euclid(7);
        let first_sunday = 1 + (7 - first_weekday_sunday_zero) % 7;
        day_of_month < first_sunday || (day_of_month == first_sunday && minute < 7 * 60)
    }
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 {
        z / 146_097
    } else {
        (z - 146_096) / 146_097
    };
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524
        - day_of_era / 146_096)
        / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era
        - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day_of_month = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    if month <= 2 {
        year += 1;
    }
    (year, month, day_of_month)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let adjusted_year = year - if month <= 2 { 1 } else { 0 };
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

export_strategy!(CmeReopeningGapRecovery::new());
