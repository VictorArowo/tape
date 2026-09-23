use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta,
    TimeInForce, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const OPEN_MINUTE: i64 = 9 * 60 + 30;
const SIGNAL_MINUTE: i64 = 9 * 60 + 59;
const EXIT_DELAY_NS: i64 = TimeDelta::from_hours(4).as_nanos();

// Days since the Unix epoch, using the proleptic Gregorian calendar.
fn days_from_civil(mut year: i64, month: i64, day: i64) -> i64 {
    year -= if month <= 2 { 1 } else { 0 };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = 365 * year_of_era + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146097 + day_of_era - 719468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let day_of_era = z - era * 146097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

// Sunday is weekday 3 with the SDK's convention that Thursday is 0.
fn new_york_dst_bounds_utc_minutes(year: i64) -> (i64, i64) {
    let march_first = days_from_civil(year, 3, 1);
    let march_first_sunday = 1 + (3 - march_first.rem_euclid(7)).rem_euclid(7);
    let second_sunday_march = march_first_sunday + 7;

    let november_first = days_from_civil(year, 11, 1);
    let first_sunday_november = 1 + (3 - november_first.rem_euclid(7)).rem_euclid(7);

    // 02:00 local standard time is 07:00 UTC; 02:00 local daylight time is 06:00 UTC.
    let start = days_from_civil(year, 3, second_sunday_march) * 1440 + 7 * 60;
    let end = days_from_civil(year, 11, first_sunday_november) * 1440 + 6 * 60;
    (start, end)
}

fn new_york_local_day_and_minute(ts_ns: i64) -> (i64, i64, i64) {
    let minute_ns = TimeDelta::from_mins(1).as_nanos();
    let utc_minute = ts_ns.div_euclid(minute_ns);
    let utc_day = utc_minute.div_euclid(1440);
    let (year, _, _) = civil_from_days(utc_day);
    let (dst_start, dst_end) = new_york_dst_bounds_utc_minutes(year);
    let is_dst = utc_minute >= dst_start && utc_minute < dst_end;
    let local_minute = utc_minute + if is_dst { -4 * 60 } else { -5 * 60 };
    let local_day = local_minute.div_euclid(1440);
    let minute_of_day = local_minute.rem_euclid(1440);
    let weekday = local_day.rem_euclid(7);
    (local_day, minute_of_day, weekday)
}

fn participation_cap(volume: Qty) -> Qty {
    volume.mul_ratio(1, 10)
}

fn smaller_qty(a: Qty, b: Qty) -> Qty {
    Qty::from_raw(a.raw().min(b.raw()))
}

struct UsOpenImpulseContinuation {
    session_day: Option<i64>,
    session_open: Option<Price>,
    used_day: Option<i64>,
    pending_entry: bool,
    entry_submit_close_ns: Option<i64>,
    entry_fill_ns: Option<i64>,
}

impl Strategy for UsOpenImpulseContinuation {
    fn name(&self) -> String {
        "US-Open Impulse Continuation".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let (local_day, minute, weekday) = new_york_local_day_and_minute(bar.open_ts.as_nanos());
        if self.session_day != Some(local_day) {
            self.session_day = Some(local_day);
            self.session_open = None;
        }

        if self.pending_entry {
            if let Some(submitted_at) = self.entry_submit_close_ns {
                if bar.close_ts.as_nanos() > submitted_at {
                    self.pending_entry = false;
                    self.entry_submit_close_ns = None;
                }
            }
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_zero() {
            self.entry_fill_ns = None;
        } else if position.is_positive() {
            if let Some(entry_ns) = self.entry_fill_ns {
                let exit_due_ns = entry_ns.saturating_add(EXIT_DELAY_NS);
                if bar.open_ts.as_nanos() >= exit_due_ns && ctx.open_orders() == 0 {
                    // Orders are IOC; any unsold amount is reconsidered on later bars.
                    let qty = smaller_qty(position, participation_cap(bar.volume));
                    if qty.is_positive() {
                        ctx.submit(
                            OrderRequest::market(INSTRUMENT, Side::Sell, qty)
                                .with_time_in_force(TimeInForce::Ioc),
                        );
                    }
                }
            }
            return;
        }

        if minute == OPEN_MINUTE {
            self.session_open = Some(bar.open);
            return;
        }

        if minute != SIGNAL_MINUTE
            || weekday == 2
            || weekday == 3
            || self.used_day == Some(local_day)
        {
            return;
        }

        let Some(session_open) = self.session_open else {
            return;
        };
        if !session_open.is_positive() || !bar.close.is_positive() {
            return;
        }

        // C/P - 1 >= 0.35%, compared exactly in fixed-point integer arithmetic.
        let impulse = i128::from(bar.close.raw()) * 10_000
            >= i128::from(session_open.raw()) * 10_035;
        if !impulse || ctx.open_orders() != 0 || self.pending_entry {
            return;
        }

        // Consume the date's entry opportunity even if sizing yields no positive order.
        self.used_day = Some(local_day);
        let budget = ctx.cash().mul_ratio(95, 100);
        let cash_qty = ctx.affordable_qty(INSTRUMENT, bar.close, budget, SIZING_MARGIN_PPM);
        let qty = smaller_qty(cash_qty, participation_cap(bar.volume));
        if qty.is_positive() {
            ctx.submit(
                OrderRequest::market(INSTRUMENT, Side::Buy, qty)
                    .with_time_in_force(TimeInForce::Ioc),
            );
            self.pending_entry = true;
            self.entry_submit_close_ns = Some(bar.close_ts.as_nanos());
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.side == Side::Buy && self.entry_fill_ns.is_none() {
            self.entry_fill_ns = Some(fill.ts.as_nanos());
        }
    }
}

export_strategy!(UsOpenImpulseContinuation {
    session_day: None,
    session_open: None,
    used_day: None,
    pending_entry: false,
    entry_submit_close_ns: None,
    entry_fill_ns: None,
});
