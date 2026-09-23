use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const FRIDAY: i64 = 1;
const SATURDAY: i64 = 2;
const PRE_EXPIRY_BARS: usize = 480;
const EXPIRY_BARS: usize = 30;
const FIRST_EXPIRY_MINUTE: i64 = 480;
const SIGNAL_BAR_MINUTE: i64 = 509;
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy)]
struct FridayVolume {
    day: i64,
    total: i128,
}

struct PostExpiryRangeBreak {
    friday_day: Option<i64>,
    pre_expiry_high: Option<tape_guest::Price>,
    pre_expiry_count: usize,
    expiry_volume: i128,
    expiry_count: usize,
    signal_processed: bool,
    prior_fridays: VecDeque<FridayVolume>,
}

impl PostExpiryRangeBreak {
    fn reset_friday(&mut self, day: i64) {
        self.friday_day = Some(day);
        self.pre_expiry_high = None;
        self.pre_expiry_count = 0;
        self.expiry_volume = 0;
        self.expiry_count = 0;
        self.signal_processed = false;
    }

    fn prior_eight_friday_median(&self, current_day: i64) -> Option<i128> {
        if self.prior_fridays.len() != 8 {
            return None;
        }

        let mut expected_day = current_day - 56;
        for friday in &self.prior_fridays {
            if friday.day != expected_day {
                return None;
            }
            expected_day += 7;
        }

        let mut volumes: Vec<i128> = self.prior_fridays.iter().map(|f| f.total).collect();
        volumes.sort_unstable();
        Some(volumes[3] + volumes[4])
    }

    fn remember_friday(&mut self, day: i64) {
        if self.expiry_count != EXPIRY_BARS {
            return;
        }
        if self.prior_fridays.len() == 8 {
            self.prior_fridays.pop_front();
        }
        self.prior_fridays.push_back(FridayVolume {
            day,
            total: self.expiry_volume,
        });
    }
}

impl Strategy for PostExpiryRangeBreak {
    fn name(&self) -> String {
        "post-expiry-range-break".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let timestamp = bar.open_ts.as_nanos();
        let day = timestamp.div_euclid(day_ns);
        let minute = timestamp.rem_euclid(day_ns).div_euclid(minute_ns);
        let weekday = day.rem_euclid(7);

        if weekday == FRIDAY {
            if self.friday_day != Some(day) {
                self.reset_friday(day);
            }

            if (0..480).contains(&minute) {
                self.pre_expiry_count += 1;
                self.pre_expiry_high = Some(match self.pre_expiry_high {
                    Some(high) if high >= bar.high => high,
                    _ => bar.high,
                });
            } else if (FIRST_EXPIRY_MINUTE..=SIGNAL_BAR_MINUTE).contains(&minute) {
                self.expiry_volume += i128::from(bar.volume.raw());
                self.expiry_count += 1;
            }

            if minute == SIGNAL_BAR_MINUTE && !self.signal_processed {
                self.signal_processed = true;
                let median_twice = self.prior_eight_friday_median(day);
                let range_is_complete = self.pre_expiry_count == PRE_EXPIRY_BARS;
                let expiry_window_is_complete = self.expiry_count == EXPIRY_BARS;
                let broke_range = self
                    .pre_expiry_high
                    .is_some_and(|high| bar.close > high);
                let volume_is_elevated = median_twice
                    .is_some_and(|median_sum| self.expiry_volume * 2 > median_sum);

                if range_is_complete
                    && expiry_window_is_complete
                    && broke_range
                    && volume_is_elevated
                    && ctx.open_orders() == 0
                    && ctx.position(INSTRUMENT).is_zero()
                {
                    let budget = ctx.cash().mul_ratio(95, 100);
                    if budget.is_positive() {
                        let quantity = ctx.affordable_qty(
                            INSTRUMENT,
                            bar.close,
                            budget,
                            SIZING_MARGIN_PPM,
                        );
                        if quantity.is_positive() {
                            ctx.submit(OrderRequest::market(
                                INSTRUMENT,
                                Side::Buy,
                                quantity,
                            ));
                        }
                    }
                }

                self.remember_friday(day);
            }
        } else if weekday == SATURDAY && minute == SIGNAL_BAR_MINUTE {
            let position = ctx.position(INSTRUMENT);
            if ctx.open_orders() == 0 && position.is_positive() {
                ctx.submit(OrderRequest::market(
                    INSTRUMENT,
                    Side::Sell,
                    position,
                ));
            }
        }
    }
}

export_strategy!(PostExpiryRangeBreak {
    friday_day: None,
    pre_expiry_high: None,
    pre_expiry_count: 0,
    expiry_volume: 0,
    expiry_count: 0,
    signal_processed: false,
    prior_fridays: VecDeque::with_capacity(8),
});
