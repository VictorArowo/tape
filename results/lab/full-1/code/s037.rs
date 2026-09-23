use tape_guest::{
    export_strategy, Bar, Ctx, Fill, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const REFERENCE_CUTOFF_MINUTE: i64 = 16 * 60;
const SUNDAY_ENTRY_MINUTE: i64 = 17 * 60 + 15;

struct LocalTime {
    day: i64,
    minute: i64,
    weekday: i64, // Sunday = 0
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 {
        z
    } else {
        z - 146_096
    }
    .div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_part = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = month_part + if month_part < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn sunday_weekday(days: i64) -> i64 {
    (days + 4).rem_euclid(7)
}

fn nth_sunday(year: i64, month: i64, occurrence: i64) -> i64 {
    let first_day = days_from_civil(year, month, 1);
    let first_sunday = 1 + (7 - sunday_weekday(first_day)).rem_euclid(7);
    first_sunday + 7 * (occurrence - 1)
}

fn chicago_time(timestamp_ns: i64) -> LocalTime {
    let minute_ns = TimeDelta::from_mins(1).as_nanos();
    let utc_minute = timestamp_ns.div_euclid(minute_ns);
    let utc_day = utc_minute.div_euclid(1_440);
    let (year, _, _) = civil_from_days(utc_day);

    // Modern U.S. daylight-saving rule: second Sunday in March through
    // first Sunday in November, with transitions at 02:00 local time.
    let spring_day = days_from_civil(year, 3, nth_sunday(year, 3, 2));
    let fall_day = days_from_civil(year, 11, nth_sunday(year, 11, 1));
    let spring_utc_minute = spring_day * 1_440 + 8 * 60;
    let fall_utc_minute = fall_day * 1_440 + 7 * 60;
    let is_dst = utc_minute >= spring_utc_minute && utc_minute < fall_utc_minute;
    let offset_minutes = if is_dst { -5 * 60 } else { -6 * 60 };

    let local_minute = utc_minute + offset_minutes;
    let day = local_minute.div_euclid(1_440);
    LocalTime {
        day,
        minute: local_minute.rem_euclid(1_440),
        weekday: sunday_weekday(day),
    }
}

fn monday_week_id(day: i64, weekday: i64) -> i64 {
    day - (weekday + 6).rem_euclid(7)
}

struct WeeklyReopenGapReversion {
    reference: Option<(i64, i64)>, // (Monday week id, reference close raw)
    used_week: Option<i64>,
    entry_price_raw: Option<i64>,
    entry_time_ns: Option<i64>,
    open_qty_raw: i64,
}

impl WeeklyReopenGapReversion {
    fn reset_entry(&mut self) {
        self.entry_price_raw = None;
        self.entry_time_ns = None;
        self.open_qty_raw = 0;
    }

    fn try_enter(&mut self, ctx: &mut Ctx, bar: &Bar, week_id: i64, reference_raw: i64) {
        if ctx.open_orders() != 0
            || !ctx.position(INSTRUMENT).is_zero()
            || self.used_week == Some(week_id)
            || reference_raw <= 0
            || bar.close.raw() <= 0
        {
            return;
        }

        let gap_raw = i128::from(reference_raw) - i128::from(bar.close.raw());
        let scaled_gap = gap_raw * 10_000;
        let scaled_reference = i128::from(reference_raw);
        if gap_raw <= 0
            || scaled_gap < scaled_reference * 75
            || scaled_gap > scaled_reference * 300
        {
            return;
        }

        // The next bar's open and volume are not yet observable. Use the
        // completed signal bar's volume as a causal proxy for the 10% cap,
        // and allow a 0.5% price cushion when sizing for slippage.
        let volume_cap = bar.volume.mul_ratio(1, 10);
        let sizing_price = bar.close.mul_ratio(10_050, 10_000);
        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            sizing_price,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        let qty = Qty::from_raw(affordable.raw().min(volume_cap.raw()));
        if !qty.is_positive() {
            return;
        }

        if ctx.open_orders() == 0 {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            // Count a submitted attempt as this week's sole entry.
            self.used_week = Some(week_id);
        }
    }
}

impl Strategy for WeeklyReopenGapReversion {
    fn name(&self) -> String {
        "weekly-reopen-gap-reversion".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let local = chicago_time(bar.close_ts.as_nanos());
        let week_id = monday_week_id(local.day, local.weekday);

        // Updating through 16:00 captures the last completed Friday bar at or
        // before the reference cutoff.
        if local.weekday == 5 && local.minute <= REFERENCE_CUTOFF_MINUTE {
            self.reference = Some((week_id, bar.close.raw()));
        }

        if local.weekday == 0 && local.minute == SUNDAY_ENTRY_MINUTE {
            if let Some((reference_week, reference_raw)) = self.reference {
                if reference_week == week_id {
                    self.try_enter(ctx, bar, week_id, reference_raw);
                }
            }
        }

        if ctx.position(INSTRUMENT).is_zero() || ctx.open_orders() != 0 {
            return;
        }

        let (Some(entry_raw), Some(entry_time_ns)) =
            (self.entry_price_raw, self.entry_time_ns)
        else {
            return;
        };
        let stop_reached = i128::from(bar.close.raw()) * 10_000
            <= i128::from(entry_raw) * 9_850;
        let target_reached = self
            .reference
            .is_some_and(|(_, reference_raw)| bar.close.raw() >= reference_raw);
        let hold_ns = TimeDelta::from_hours(48).as_nanos();
        let time_reached = bar.close_ts.as_nanos() >= entry_time_ns.saturating_add(hold_ns);

        if target_reached || stop_reached || time_reached {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() && ctx.open_orders() == 0 {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Buy {
            let added_qty = fill.qty.raw();
            if added_qty <= 0 {
                return;
            }
            if self.open_qty_raw <= 0 {
                self.entry_price_raw = Some(fill.price.raw());
                self.entry_time_ns = Some(fill.ts.as_nanos());
                self.open_qty_raw = added_qty;
            } else {
                let total_qty = self.open_qty_raw.saturating_add(added_qty);
                let weighted_price = i128::from(self.entry_price_raw.unwrap_or(fill.price.raw()))
                    * i128::from(self.open_qty_raw)
                    + i128::from(fill.price.raw()) * i128::from(added_qty);
                self.entry_price_raw = Some((weighted_price / i128::from(total_qty)) as i64);
                self.open_qty_raw = total_qty;
            }
        } else if fill.side == Side::Sell {
            self.open_qty_raw = self.open_qty_raw.saturating_sub(fill.qty.raw());
            if self.open_qty_raw <= 0 {
                self.reset_entry();
            }
        }
    }
}

export_strategy!(WeeklyReopenGapReversion {
    reference: None,
    used_week: None,
    entry_price_raw: None,
    entry_time_ns: None,
    open_qty_raw: 0,
});
