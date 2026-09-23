use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, Fill, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const LOOKBACK: usize = 20;
const SIZING_MARGIN_PPM: i64 = 5_000;

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    if month <= 2 {
        year += 1;
    }
    (year, month, day)
}

// Weekday numbering is 0 = Thursday, matching the SDK description.
fn weekday(day_number: i64) -> i64 {
    day_number.rem_euclid(7)
}

fn first_sunday(year: i64, month: i64) -> i64 {
    let y = year - if month <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let day_number = era * 146_097 + doe - 719_468;
    1 + (3 - weekday(day_number)).rem_euclid(7)
}

fn is_us_dst(day_number: i64) -> bool {
    let (year, month, day) = civil_from_days(day_number);
    if month < 3 || month > 11 {
        return false;
    }
    if month > 3 && month < 11 {
        return true;
    }
    if month == 3 {
        return day >= first_sunday(year, 3) + 7;
    }
    day < first_sunday(year, 11)
}

fn next_weekday(day: i64) -> i64 {
    let mut next = day + 1;
    while weekday(next) == 2 || weekday(next) == 3 {
        next += 1;
    }
    next
}

struct FlowCarry {
    current_day: Option<i64>,
    session_open: Option<i64>,
    session_volume: i128,
    history: VecDeque<i128>,
    evaluated_day: Option<i64>,
    target_exit_day: Option<i64>,
    entry_price: Option<i64>,
}

impl FlowCarry {
    fn new() -> Self {
        Self {
            current_day: None,
            session_open: None,
            session_volume: 0,
            history: VecDeque::with_capacity(LOOKBACK),
            evaluated_day: None,
            target_exit_day: None,
            entry_price: None,
        }
    }

    fn evaluate_session(&mut self, ctx: &mut Ctx, day: i64, close: i64) {
        if self.evaluated_day == Some(day) || self.session_open.is_none() {
            return;
        }
        self.evaluated_day = Some(day);
        let volume = self.session_volume;

        if ctx.position(INSTRUMENT).is_zero()
            && ctx.open_orders() == 0
            && self.history.len() == LOOKBACK
        {
            let mean_volume = self.history.iter().sum::<i128>() / LOOKBACK as i128;
            let open = self.session_open.unwrap_or(close);
            let return_ok = i128::from(close) * 1_000 >= i128::from(open) * 1_005;
            let volume_ok = mean_volume > 0 && volume * 4 >= mean_volume * 5;
            if return_ok && volume_ok {
                let price = tape_guest::Price::from_raw(close);
                let qty = ctx.affordable_qty(
                    INSTRUMENT,
                    price,
                    ctx.cash(),
                    SIZING_MARGIN_PPM,
                );
                if qty.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                    self.target_exit_day = Some(next_weekday(day));
                }
            }
        }

        self.history.push_back(volume);
        if self.history.len() > LOOKBACK {
            self.history.pop_front();
        }
    }
}

impl Strategy for FlowCarry {
    fn name(&self) -> String {
        "ETF-Session Flow Carry".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let day_ns = TimeDelta::from_days(1).as_nanos();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let ts = bar.close_ts.as_nanos();
        let day = ts.div_euclid(day_ns);
        let minute = ts.rem_euclid(day_ns) / minute_ns;
        let start = if is_us_dst(day) { 15 * 60 + 30 } else { 14 * 60 + 30 };
        let end = start + 6 * 60 + 30;

        if self.current_day != Some(day) {
            self.current_day = Some(day);
            self.session_open = None;
            self.session_volume = 0;
        }

        // These completed bars cover the session from its opening bar up to,
        // but not including, the bar closing at the cash-session end.
        if minute >= start && minute < end {
            if self.session_open.is_none() {
                self.session_open = Some(bar.open.raw());
            }
            self.session_volume += i128::from(bar.volume.raw());
        }

        // At the close-time boundary, use the last completed in-session close.
        if minute == end {
            self.evaluate_session(ctx, day, bar.open.raw());
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() && ctx.open_orders() == 0 {
            let stop_hit = self.entry_price.is_some_and(|entry| {
                i128::from(bar.close.raw()) * 1_000 <= i128::from(entry) * 980
            });
            let scheduled_exit = self
                .target_exit_day
                .is_some_and(|target| day >= target && minute >= start);
            if stop_hit || scheduled_exit {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Buy {
            self.entry_price = Some(fill.price.raw());
        } else {
            self.entry_price = None;
            self.target_exit_day = None;
        }
    }
}

export_strategy!(FlowCarry::new());
