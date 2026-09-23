use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const SIGNAL_MINUTE: i64 = 12 * 60;
const RETURN_DAYS: i64 = 30;

struct MonthEndRebalancePause {
    closes: VecDeque<(i64, i64)>,
    buy_next_month: bool,
}

fn civil_date(day_number: i64) -> (i64, i64, i64) {
    // Convert days since the Unix epoch to a Gregorian year, month, and day.
    let z = day_number + 719_468;
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
    let month_part = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = if month_part < 10 {
        month_part + 3
    } else {
        month_part - 9
    };
    if month <= 2 {
        year += 1;
    }
    (year, month, day)
}

fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

impl Strategy for MonthEndRebalancePause {
    fn name(&self) -> String {
        "month-end-rebalance-pause".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let close_ts = bar.close_ts.as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let target_ts = close_ts.saturating_sub(day_ns.saturating_mul(RETURN_DAYS));

        self.closes.push_back((close_ts, bar.close.raw()));
        while self.closes.len() >= 2 {
            let next_ts = self.closes.get(1).expect("length checked").0;
            if next_ts <= target_ts {
                self.closes.pop_front();
            } else {
                break;
            }
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let minute_of_day = close_ts.div_euclid(minute_ns).rem_euclid(1_440);
        if minute_of_day != SIGNAL_MINUTE {
            return;
        }

        let day_number = close_ts.div_euclid(day_ns);
        let (year, month, day) = civil_date(day_number);
        let final_day = day == days_in_month(year, month);

        if day == 1 && self.buy_next_month {
            self.buy_next_month = false;
            if ctx.open_orders() == 0 {
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

        if final_day {
            let reference = self
                .closes
                .front()
                .filter(|(ts, price)| *ts <= target_ts && *price > 0)
                .map(|(_, price)| *price);

            if let Some(reference_close) = reference {
                if bar.close.raw() > reference_close {
                    self.buy_next_month = true;
                    let position = ctx.position(INSTRUMENT);
                    if position.is_positive() && ctx.open_orders() == 0 {
                        ctx.submit(OrderRequest::market(
                            INSTRUMENT,
                            Side::Sell,
                            position,
                        ));
                    }
                }
            }
        }
    }
}

export_strategy!(MonthEndRebalancePause {
    closes: VecDeque::new(),
    buy_next_month: false,
});
