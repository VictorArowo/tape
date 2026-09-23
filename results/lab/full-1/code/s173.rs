use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, Timestamp, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const VOLUME_CAP_DENOMINATOR: i64 = 10;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Flat,
    Entering,
    Long,
    Exiting,
}

struct LondonFixPressureFade {
    mode: Mode,
    fix_day: Option<i64>,
    fix_open: Option<Price>,
    flat_at_fix_start: bool,
    last_checked_fix_day: Option<i64>,

    entry_target_raw: i64,
    entry_filled_raw: i64,
    entry_notional: i128,
    entry_vwap_raw: i64,
    held_bars: i64,

    exit_target_raw: i64,
    exit_filled_raw: i64,
}

impl LondonFixPressureFade {
    fn new() -> Self {
        Self {
            mode: Mode::Flat,
            fix_day: None,
            fix_open: None,
            flat_at_fix_start: false,
            last_checked_fix_day: None,
            entry_target_raw: 0,
            entry_filled_raw: 0,
            entry_notional: 0,
            entry_vwap_raw: 0,
            held_bars: 0,
            exit_target_raw: 0,
            exit_filled_raw: 0,
        }
    }

    fn submit_entry_slice(&self, ctx: &mut Ctx, bar: &Bar) {
        if ctx.open_orders() != 0 {
            return;
        }
        let remaining = self.entry_target_raw - self.entry_filled_raw;
        if remaining <= 0 {
            return;
        }
        let volume_cap = bar.volume.mul_ratio(1, VOLUME_CAP_DENOMINATOR).raw();
        if volume_cap <= 0 {
            return;
        }
        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        ).raw();
        let qty_raw = remaining.min(volume_cap).min(affordable);
        if qty_raw > 0 {
            ctx.submit(OrderRequest::market(
                INSTRUMENT,
                Side::Buy,
                tape_guest::Qty::from_raw(qty_raw),
            ));
        }
    }

    fn submit_exit_slice(&self, ctx: &mut Ctx, bar: &Bar) {
        if ctx.open_orders() != 0 {
            return;
        }
        let remaining = self.exit_target_raw - self.exit_filled_raw;
        let position_raw = ctx.position(INSTRUMENT).raw();
        if remaining <= 0 || position_raw <= 0 {
            return;
        }
        let volume_cap = bar.volume.mul_ratio(1, VOLUME_CAP_DENOMINATOR).raw();
        let qty_raw = remaining.min(position_raw).min(volume_cap);
        if qty_raw > 0 {
            ctx.submit(OrderRequest::market(
                INSTRUMENT,
                Side::Sell,
                tape_guest::Qty::from_raw(qty_raw),
            ));
        }
    }

    fn reset_position_state(&mut self) {
        self.mode = Mode::Flat;
        self.entry_target_raw = 0;
        self.entry_filled_raw = 0;
        self.entry_notional = 0;
        self.entry_vwap_raw = 0;
        self.held_bars = 0;
        self.exit_target_raw = 0;
        self.exit_filled_raw = 0;
    }
}

impl Strategy for LondonFixPressureFade {
    fn name(&self) -> String {
        String::from("London Fix Pressure Fade")
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let utc_minute = bar.close_ts.as_nanos().div_euclid(minute_ns);
        let (local_day, local_minute) = london_local_minute(utc_minute, bar.close_ts);

        if local_minute == 15 * 60 {
            self.fix_day = Some(local_day);
            self.fix_open = Some(bar.open);
            self.flat_at_fix_start = self.mode == Mode::Flat
                && ctx.position(INSTRUMENT).is_zero()
                && ctx.open_orders() == 0;
        }

        let mut enter_now = false;
        if local_minute == 15 * 60 + 59
            && self.last_checked_fix_day != Some(local_day)
        {
            self.last_checked_fix_day = Some(local_day);
            if self.fix_day == Some(local_day)
                && self.flat_at_fix_start
                && self.mode == Mode::Flat
                && ctx.position(INSTRUMENT).is_zero()
            {
                if let Some(fix_open) = self.fix_open {
                    let lhs = i128::from(bar.close.raw()) * 10_000;
                    let rhs = i128::from(fix_open.raw()) * 9_970;
                    enter_now = fix_open.is_positive() && lhs <= rhs;
                }
            }
        }

        if ctx.open_orders() != 0 {
            return;
        }

        match self.mode {
            Mode::Flat => {
                if enter_now {
                    let target = ctx.affordable_qty(
                        INSTRUMENT,
                        bar.close,
                        ctx.cash(),
                        SIZING_MARGIN_PPM,
                    );
                    if target.is_positive() {
                        self.entry_target_raw = target.raw();
                        self.entry_filled_raw = 0;
                        self.entry_notional = 0;
                        self.entry_vwap_raw = 0;
                        self.mode = Mode::Entering;
                        self.submit_entry_slice(ctx, bar);
                    }
                }
            }
            Mode::Entering => {
                self.submit_entry_slice(ctx, bar);
            }
            Mode::Long => {
                self.held_bars += 1;
                let close_scaled = i128::from(bar.close.raw()) * 10_000;
                let entry_scaled = i128::from(self.entry_vwap_raw);
                let take_profit = close_scaled >= entry_scaled * 10_045;
                let stop_loss = close_scaled <= entry_scaled * 9_940;
                let time_exit = self.held_bars >= 180;
                if take_profit || stop_loss || time_exit {
                    let position = ctx.position(INSTRUMENT);
                    if position.is_positive() {
                        self.exit_target_raw = position.raw();
                        self.exit_filled_raw = 0;
                        self.mode = Mode::Exiting;
                        self.submit_exit_slice(ctx, bar);
                    } else {
                        self.reset_position_state();
                    }
                }
            }
            Mode::Exiting => {
                if ctx.position(INSTRUMENT).is_zero() {
                    self.reset_position_state();
                } else {
                    self.submit_exit_slice(ctx, bar);
                }
            }
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        match (self.mode, fill.side) {
            (Mode::Entering, Side::Buy) => {
                let qty_raw = fill.qty.raw();
                self.entry_filled_raw += qty_raw;
                self.entry_notional += i128::from(fill.price.raw()) * i128::from(qty_raw);
                if self.entry_filled_raw > 0 {
                    self.entry_vwap_raw =
                        (self.entry_notional / i128::from(self.entry_filled_raw)) as i64;
                }
                if self.entry_filled_raw >= self.entry_target_raw {
                    self.mode = Mode::Long;
                    self.held_bars = 0;
                }
            }
            (Mode::Exiting, Side::Sell) => {
                self.exit_filled_raw += fill.qty.raw();
                if self.exit_filled_raw >= self.exit_target_raw {
                    self.reset_position_state();
                }
            }
            _ => {}
        }
    }
}

// Unix day zero is Thursday. Europe/London is UTC+1 between the last Sundays
// of March and October, with both transitions occurring at 01:00 UTC.
fn london_local_minute(utc_minute: i64, ts: Timestamp) -> (i64, i64) {
    let utc_day = utc_minute.div_euclid(1_440);
    let (year, _, _) = civil_from_days(utc_day);
    let start = last_sunday_of_month(year, 3) * 1_440 + 60;
    let end = last_sunday_of_month(year, 10) * 1_440 + 60;
    let summer_time = utc_minute >= start && utc_minute < end;
    let offset = if summer_time { 60 } else { 0 };
    let local_minute = utc_minute + offset;
    let _ = ts;
    (local_minute.div_euclid(1_440), local_minute.rem_euclid(1_440))
}

fn last_sunday_of_month(year: i64, month: i64) -> i64 {
    let next_month = if month == 12 {
        days_from_civil(year + 1, 1, 1)
    } else {
        days_from_civil(year, month + 1, 1)
    };
    let last_day = next_month - 1;
    let weekday = last_day.rem_euclid(7); // 0 = Thursday, 3 = Sunday
    last_day - (weekday - 3).rem_euclid(7)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let adjusted_year = year - if month <= 2 { 1 } else { 0 };
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = 365 * year_of_era + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
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

export_strategy!(LondonFixPressureFade::new());
