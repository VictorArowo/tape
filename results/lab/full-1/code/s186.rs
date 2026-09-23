use tape_guest::{
    Bar, Ctx, InstrumentId, OrderId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta,
    Timestamp, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const FRIDAY_CLOSE_MINUTE: i64 = 16 * 60;
const SUNDAY_REOPEN_MINUTE: i64 = 17 * 60;

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

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let shifted = days_since_epoch + 719_468;
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
    let month_part = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = month_part + if month_part < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

fn sunday_on_or_after_first(year: i64, month: i64) -> i64 {
    let first_day = days_from_civil(year, month, 1);
    let weekday = first_day.rem_euclid(7); // 0 = Thursday, 3 = Sunday
    1 + (3 - weekday).rem_euclid(7)
}

fn chicago_day_and_minute(ts: Timestamp) -> (i64, i64) {
    let minute_ns = TimeDelta::from_mins(1).as_nanos();
    let utc_minute = ts.as_nanos().div_euclid(minute_ns);
    let utc_day = utc_minute.div_euclid(1_440);
    let (year, _, _) = civil_from_days(utc_day);

    // U.S. DST begins at 08:00 UTC on the second Sunday in March and ends
    // at 07:00 UTC on the first Sunday in November.
    let march_sunday = sunday_on_or_after_first(year, 3) + 7;
    let november_sunday = sunday_on_or_after_first(year, 11);
    let dst_start = days_from_civil(year, 3, march_sunday) * 1_440 + 8 * 60;
    let dst_end = days_from_civil(year, 11, november_sunday) * 1_440 + 7 * 60;
    let offset_minutes = if utc_minute >= dst_start && utc_minute < dst_end {
        5 * 60
    } else {
        6 * 60
    };

    let local_minute = utc_minute - offset_minutes;
    (
        local_minute.div_euclid(1_440),
        local_minute.rem_euclid(1_440),
    )
}

struct CmeWeekendGapReclaim {
    friday_reference: Option<Price>,
    friday_day: Option<i64>,
    pending_reference: Option<Price>,
    entry_order: Option<OrderId>,
    entry_pending: bool,
    entry_price: Option<Price>,
    entry_ts_ns: Option<i64>,
    trade_reference: Option<Price>,
    target_order: Option<OrderId>,
    target_remaining_raw: i64,
    exit_pending: bool,
    target_cancel_requested: bool,
}

impl CmeWeekendGapReclaim {
    fn new() -> Self {
        Self {
            friday_reference: None,
            friday_day: None,
            pending_reference: None,
            entry_order: None,
            entry_pending: false,
            entry_price: None,
            entry_ts_ns: None,
            trade_reference: None,
            target_order: None,
            target_remaining_raw: 0,
            exit_pending: false,
            target_cancel_requested: false,
        }
    }

    fn clear_trade(&mut self) {
        self.pending_reference = None;
        self.entry_order = None;
        self.entry_pending = false;
        self.entry_price = None;
        self.entry_ts_ns = None;
        self.trade_reference = None;
        self.target_order = None;
        self.target_remaining_raw = 0;
        self.exit_pending = false;
        self.target_cancel_requested = false;
    }
}

impl Strategy for CmeWeekendGapReclaim {
    fn name(&self) -> String {
        "CME Weekend Gap Reclaim".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let (local_day, local_minute) = chicago_day_and_minute(bar.close_ts);
        let weekday = local_day.rem_euclid(7); // 0 = Thursday, 1 = Friday, 3 = Sunday

        if weekday == 1 && local_minute == FRIDAY_CLOSE_MINUTE {
            self.friday_reference = Some(bar.close);
            self.friday_day = Some(local_day);
        }

        if self.entry_price.is_some() {
            if ctx.position(INSTRUMENT).is_zero() {
                self.clear_trade();
                return;
            }

            let entry_price = self.entry_price.expect("active trade has an entry price");
            let stop_price = entry_price.mul_ratio(9_900, 10_000);
            let stop_hit = bar.low <= stop_price;
            let timed_out = self.entry_ts_ns.is_some_and(|entry_ts| {
                i128::from(bar.close_ts.as_nanos())
                    >= i128::from(entry_ts)
                        + i128::from(TimeDelta::from_hours(48).as_nanos())
            });

            if stop_hit || timed_out {
                self.exit_pending = true;
            }

            if self.exit_pending {
                if !self.target_cancel_requested {
                    if let Some(order_id) = self.target_order {
                        ctx.cancel(order_id);
                    }
                    self.target_cancel_requested = true;
                }
                if ctx.open_orders() == 0 {
                    let position = ctx.position(INSTRUMENT);
                    if position.is_positive() {
                        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                    }
                }
                return;
            }

            if self.target_order.is_none() && ctx.open_orders() == 0 {
                if let (Some(reference), position) =
                    (self.trade_reference, ctx.position(INSTRUMENT))
                {
                    if position.is_positive() {
                        let order_id = ctx.submit(OrderRequest::limit(
                            INSTRUMENT,
                            Side::Sell,
                            position,
                            reference,
                        ));
                        self.target_order = Some(order_id);
                        self.target_remaining_raw = position.raw();
                    }
                }
            }
            return;
        }

        if self.entry_pending {
            if ctx.open_orders() == 0 && ctx.position(INSTRUMENT).is_zero() {
                self.clear_trade();
            }
            return;
        }

        if weekday != 3 || local_minute != SUNDAY_REOPEN_MINUTE {
            return;
        }
        if ctx.open_orders() != 0 || !ctx.position(INSTRUMENT).is_zero() {
            return;
        }

        let (Some(reference), Some(reference_day)) = (self.friday_reference, self.friday_day)
        else {
            return;
        };
        if local_day - reference_day != 2 {
            return;
        }

        let gap_threshold = reference.mul_ratio(9_925, 10_000);
        if bar.close > gap_threshold {
            return;
        }

        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        let volume_cap = bar.volume.mul_ratio(1, 10);
        let quantity = Qty::from_raw(affordable.raw().min(volume_cap.raw()));
        if !quantity.is_positive() {
            return;
        }

        let order_id = ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, quantity));
        self.entry_order = Some(order_id);
        self.entry_pending = true;
        self.pending_reference = Some(reference);
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.side == Side::Buy
            && self.entry_pending
            && self.entry_order == Some(fill.order_id)
        {
            self.entry_pending = false;
            self.entry_order = None;
            self.entry_price = Some(fill.price);
            self.entry_ts_ns = Some(fill.ts.as_nanos());
            self.trade_reference = self.pending_reference;
            self.pending_reference = None;
            return;
        }

        if fill.side == Side::Sell && self.entry_price.is_some() {
            if self.target_order == Some(fill.order_id) {
                self.target_remaining_raw =
                    (self.target_remaining_raw - fill.qty.raw()).max(0);
                if self.target_remaining_raw == 0 {
                    self.target_order = None;
                }
            }
            if ctx.position(INSTRUMENT).is_zero() {
                self.clear_trade();
            }
        }
    }
}

export_strategy!(CmeWeekendGapReclaim::new());
