use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const INTERVAL_MINUTES: u16 = 420;

struct AsiaEuropeHandoffReversion {
    current_day: Option<i64>,
    midnight_close_raw: Option<i64>,
    interval_volume_raw: i128,
    interval_bars: u16,
    last_processed_0700_day: Option<i64>,
    daily_volumes: VecDeque<(i64, i128)>,
}

impl AsiaEuropeHandoffReversion {
    fn new() -> Self {
        Self {
            current_day: None,
            midnight_close_raw: None,
            interval_volume_raw: 0,
            interval_bars: 0,
            last_processed_0700_day: None,
            daily_volumes: VecDeque::with_capacity(21),
        }
    }

    fn volume_below_prior_median(&self, day: i64, volume_raw: i128) -> bool {
        let first_day = day.saturating_sub(20);
        let mut prior: Vec<i128> = self
            .daily_volumes
            .iter()
            .filter_map(|(sample_day, volume)| {
                (*sample_day >= first_day && *sample_day < day).then_some(*volume)
            })
            .collect();

        if prior.len() != 20 {
            return false;
        }

        prior.sort_unstable();
        // For 20 observations, the median is the mean of the two middle values.
        // volume < 1.5 * median is exactly 4*volume < 3*(middle_a + middle_b).
        volume_raw * 4 < 3 * (prior[9] + prior[10])
    }

    fn remember_daily_volume(&mut self, day: i64, volume_raw: i128) {
        while self
            .daily_volumes
            .front()
            .is_some_and(|(sample_day, _)| *sample_day < day.saturating_sub(20))
        {
            self.daily_volumes.pop_front();
        }
        self.daily_volumes.push_back((day, volume_raw));
    }
}

impl Strategy for AsiaEuropeHandoffReversion {
    fn name(&self) -> String {
        "asia-europe-handoff-reversion".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let timestamp_ns = bar.close_ts.as_nanos();
        let day = timestamp_ns.div_euclid(day_ns);
        let minute_of_day = timestamp_ns
            .div_euclid(minute_ns)
            .rem_euclid(1440) as i64;

        if self.current_day != Some(day) {
            self.current_day = Some(day);
            self.midnight_close_raw = None;
            self.interval_volume_raw = 0;
            self.interval_bars = 0;
        }

        if minute_of_day == 0 {
            self.midnight_close_raw = Some(bar.close.raw());
            self.interval_volume_raw = 0;
            self.interval_bars = 0;
        } else if (1..=i64::from(INTERVAL_MINUTES)).contains(&minute_of_day) {
            self.interval_volume_raw += i128::from(bar.volume.raw());
            self.interval_bars = self.interval_bars.saturating_add(1);
        }

        if minute_of_day == i64::from(INTERVAL_MINUTES)
            && self.last_processed_0700_day != Some(day)
        {
            self.last_processed_0700_day = Some(day);
            let complete = self.midnight_close_raw.is_some()
                && self.interval_bars == INTERVAL_MINUTES;

            let return_in_range = self.midnight_close_raw.is_some_and(|start_raw| {
                if start_raw <= 0 {
                    return false;
                }
                let scaled_change = i128::from(bar.close.raw() - start_raw) * 1_000;
                let start_scaled = i128::from(start_raw);
                scaled_change >= -12 * start_scaled && scaled_change <= -4 * start_scaled
            });
            let low_volume = complete
                && self.volume_below_prior_median(day, self.interval_volume_raw);

            if complete {
                self.remember_daily_volume(day, self.interval_volume_raw);
            }

            if complete
                && return_in_range
                && low_volume
                && ctx.open_orders() == 0
                && ctx.position(INSTRUMENT).is_zero()
            {
                let qty = ctx.affordable_qty(
                    INSTRUMENT,
                    bar.close,
                    ctx.cash(),
                    SIZING_MARGIN_PPM,
                );
                if qty.is_positive() {
                    // The execution engine applies its per-bar fill cap, fees, and slippage.
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                }
            }
        }

        // The order submitted on the 13:00 close is intended to fill at the next bar's open.
        if minute_of_day == 13 * 60 && ctx.open_orders() == 0 {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
        }
    }
}

export_strategy!(AsiaEuropeHandoffReversion::new());
