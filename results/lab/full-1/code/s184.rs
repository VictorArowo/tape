use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Start,
    BuyInitial,
    Invested,
    Sell,
    AwaitReentry,
    BuyReentry,
}

struct WeekendReopenGapFade {
    phase: Phase,
    working: bool,
    filled_raw: i128,
    buy_target: Qty,
    buy_target_ready: bool,
    friday_close: Option<(i64, Price)>,
    reentry_level: Option<Price>,
    last_sale_fill_ns: Option<i64>,
    reentry_deadline_ns: Option<i64>,
}

impl WeekendReopenGapFade {
    fn new() -> Self {
        Self {
            phase: Phase::Start,
            working: false,
            filled_raw: 0,
            buy_target: Qty::from_raw(0),
            buy_target_ready: false,
            friday_close: None,
            reentry_level: None,
            last_sale_fill_ns: None,
            reentry_deadline_ns: None,
        }
    }

    fn submit_buy(&mut self, ctx: &mut Ctx, bar: &Bar) {
        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        if !self.buy_target_ready {
            if !affordable.is_positive() {
                return;
            }
            self.buy_target = affordable;
            self.buy_target_ready = true;
        }

        let qty = if self.buy_target.raw() < affordable.raw() {
            self.buy_target
        } else {
            affordable
        };
        if !qty.is_positive() {
            return;
        }

        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        self.working = true;
        self.filled_raw = 0;
    }

    fn submit_sell(&mut self, ctx: &mut Ctx) {
        let position = ctx.position(INSTRUMENT);
        if !position.is_positive() {
            return;
        }
        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
        self.working = true;
        self.filled_raw = 0;
    }
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
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

// Returns the local calendar day and minute-of-day for an epoch timestamp.
fn chicago_day_and_minute(timestamp_ns: i64) -> (i64, i64) {
    let day_ns = TimeDelta::from_days(1).as_nanos();
    let hour_ns = TimeDelta::from_hours(1).as_nanos();
    let minute_ns = TimeDelta::from_mins(1).as_nanos();
    let utc_day = timestamp_ns.div_euclid(day_ns);
    let (year, _, _) = civil_from_days(utc_day);

    // Chicago daylight time begins at 08:00 UTC on the second Sunday in March
    // and ends at 07:00 UTC on the first Sunday in November.
    let march_first = days_from_civil(year, 3, 1);
    let march_weekday = march_first.rem_euclid(7); // 0 = Thursday, 3 = Sunday
    let first_march_sunday = 1 + (3 - march_weekday).rem_euclid(7);
    let dst_start_ns = days_from_civil(year, 3, first_march_sunday + 7) * day_ns
        + 8 * hour_ns;

    let november_first = days_from_civil(year, 11, 1);
    let november_weekday = november_first.rem_euclid(7);
    let first_november_sunday = 1 + (3 - november_weekday).rem_euclid(7);
    let dst_end_ns = days_from_civil(year, 11, first_november_sunday) * day_ns
        + 7 * hour_ns;

    let offset_ns = if timestamp_ns >= dst_start_ns && timestamp_ns < dst_end_ns {
        -5 * hour_ns
    } else {
        -6 * hour_ns
    };
    let local_ns = timestamp_ns + offset_ns;
    let local_day = local_ns.div_euclid(day_ns);
    let minute_of_day = local_ns.rem_euclid(day_ns) / minute_ns;
    (local_day, minute_of_day)
}

impl Strategy for WeekendReopenGapFade {
    fn name(&self) -> String {
        "weekend-reopen-gap-fade".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        if self.working {
            if ctx.open_orders() > 0 {
                return;
            }
            self.working = false;
            if matches!(self.phase, Phase::BuyInitial | Phase::BuyReentry) {
                let remaining = (i128::from(self.buy_target.raw()) - self.filled_raw).max(0);
                self.buy_target = Qty::from_raw(remaining.min(i128::from(i64::MAX)) as i64);
                if !self.buy_target.is_positive() {
                    self.buy_target_ready = false;
                    self.phase = Phase::Invested;
                }
            }
            self.filled_raw = 0;
        }

        if ctx.open_orders() > 0 {
            return;
        }

        if self.phase == Phase::Start {
            self.phase = Phase::BuyInitial;
        }

        let (local_day, minute_of_day) = chicago_day_and_minute(bar.open_ts.as_nanos());
        let weekday = local_day.rem_euclid(7); // 0 = Thursday

        if self.phase == Phase::Invested {
            if weekday == 1 && minute_of_day == 16 * 60 {
                self.friday_close = Some((local_day, bar.close));
            }

            if weekday == 3 && minute_of_day == 17 * 60 + 30 {
                let signal = self
                    .friday_close
                    .filter(|(friday_day, _)| *friday_day + 2 == local_day)
                    .map(|(_, friday_price)| {
                        i128::from(bar.close.raw()) * 1_000
                            >= i128::from(friday_price.raw()) * 1_015
                    })
                    .unwrap_or(false);
                self.friday_close = None;

                if signal && ctx.position(INSTRUMENT).is_positive() {
                    self.reentry_level = Some(bar.close.mul_ratio(994, 1_000));
                    self.last_sale_fill_ns = None;
                    self.reentry_deadline_ns = None;
                    self.phase = Phase::Sell;
                }
            }
        }

        if self.phase == Phase::Sell && !ctx.position(INSTRUMENT).is_positive() {
            let sale_time = self.last_sale_fill_ns.unwrap_or(bar.close_ts.as_nanos());
            self.reentry_deadline_ns = Some(
                sale_time + TimeDelta::from_hours(12).as_nanos(),
            );
            self.phase = Phase::AwaitReentry;
        }

        if self.phase == Phase::AwaitReentry {
            let hit_price = self
                .reentry_level
                .map(|level| bar.close <= level)
                .unwrap_or(false);
            let timed_out = self
                .reentry_deadline_ns
                .map(|deadline| bar.close_ts.as_nanos() > deadline)
                .unwrap_or(false);
            if hit_price || timed_out {
                self.phase = Phase::BuyReentry;
                self.buy_target = Qty::from_raw(0);
                self.buy_target_ready = false;
            }
        }

        match self.phase {
            Phase::BuyInitial | Phase::BuyReentry => self.submit_buy(ctx, bar),
            Phase::Sell => self.submit_sell(ctx),
            Phase::Start | Phase::Invested | Phase::AwaitReentry => {}
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if !self.working {
            return;
        }
        match (self.phase, fill.side) {
            (Phase::BuyInitial | Phase::BuyReentry, Side::Buy) => {
                self.filled_raw += i128::from(fill.qty.raw());
            }
            (Phase::Sell, Side::Sell) => {
                self.last_sale_fill_ns = Some(fill.ts.as_nanos());
            }
            _ => {}
        }
    }
}

export_strategy!(WeekendReopenGapFade::new());
