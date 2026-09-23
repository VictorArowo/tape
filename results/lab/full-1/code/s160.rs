use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const FRIDAY: i64 = 1;
const EXPIRY_MINUTE: i64 = 8 * 60;
const SIGNAL_MINUTE: i64 = 8 * 60 + 30;
const SIZING_MARGIN_PPM: i64 = 5_000;
const HIGH_WINDOW_BARS: usize = 12 * 60;
const HISTORY_FRIDAYS: usize = 8;

struct WeeklyExpiryHedgeUnwind {
    recent_highs: VecDeque<(i64, i64)>,
    expiry_high: Option<Price>,
    friday_volumes: VecDeque<i128>,
    current_volume_day: Option<i64>,
    current_interval_volume: i128,
    current_interval_bars: usize,
    last_processed_expiry_day: Option<i64>,
    entry_ts_ns: Option<i64>,
}

impl WeeklyExpiryHedgeUnwind {
    fn new() -> Self {
        Self {
            recent_highs: VecDeque::with_capacity(HIGH_WINDOW_BARS + 1),
            expiry_high: None,
            friday_volumes: VecDeque::with_capacity(HISTORY_FRIDAYS),
            current_volume_day: None,
            current_interval_volume: 0,
            current_interval_bars: 0,
            last_processed_expiry_day: None,
            entry_ts_ns: None,
        }
    }

    fn update_high_window(&mut self, bar: &Bar, minute_ns: i64, window_ns: i64) {
        let now = bar.close_ts.as_nanos();
        let cutoff = now.saturating_sub(window_ns);
        self.recent_highs.push_back((now, bar.high.raw()));
        while self
            .recent_highs
            .front()
            .map(|(ts, _)| *ts <= cutoff)
            .unwrap_or(false)
        {
            self.recent_highs.pop_front();
        }

        let minute_after_cutoff = cutoff.saturating_add(minute_ns);
        let complete = self.recent_highs.len() == HIGH_WINDOW_BARS
            && self
                .recent_highs
                .front()
                .map(|(ts, _)| *ts == minute_after_cutoff)
                .unwrap_or(false)
            && self
                .recent_highs
                .back()
                .map(|(ts, _)| *ts == now)
                .unwrap_or(false);

        if complete {
            if let Some(max_high) = self.recent_highs.iter().map(|(_, high)| *high).max() {
                self.expiry_high = Some(Price::from_raw(max_high));
            }
        } else {
            self.expiry_high = None;
        }
    }

    fn record_friday_volume(&mut self, volume: i128) {
        self.friday_volumes.push_back(volume);
        while self.friday_volumes.len() > HISTORY_FRIDAYS {
            self.friday_volumes.pop_front();
        }
    }

    fn volume_exceeds_previous_median(&self, volume: i128) -> bool {
        if self.friday_volumes.len() < HISTORY_FRIDAYS {
            return false;
        }
        let mut sorted: Vec<i128> = self.friday_volumes.iter().copied().collect();
        sorted.sort_unstable();
        // For eight observations, the median is the mean of the two middle values.
        volume * 2 > sorted[3] + sorted[4]
    }
}

impl Strategy for WeeklyExpiryHedgeUnwind {
    fn name(&self) -> String {
        "weekly-expiry-hedge-unwind".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let window_ns = TimeDelta::from_hours(12).as_nanos();
        let now_ns = bar.close_ts.as_nanos();
        let absolute_minute = now_ns.div_euclid(minute_ns);
        let minute_of_day = absolute_minute.rem_euclid(1440);
        let day_number = now_ns.div_euclid(day_ns);
        let weekday = day_number.rem_euclid(7);

        self.update_high_window(bar, minute_ns, window_ns);

        if weekday == FRIDAY && minute_of_day == EXPIRY_MINUTE {
            self.current_volume_day = Some(day_number);
            self.current_interval_volume = 0;
            self.current_interval_bars = 0;
        }

        if weekday == FRIDAY
            && minute_of_day > EXPIRY_MINUTE
            && minute_of_day <= SIGNAL_MINUTE
            && self.current_volume_day == Some(day_number)
        {
            self.current_interval_volume += i128::from(bar.volume.raw());
            self.current_interval_bars += 1;
        }

        if weekday == FRIDAY
            && minute_of_day == SIGNAL_MINUTE
            && self.last_processed_expiry_day != Some(day_number)
        {
            self.last_processed_expiry_day = Some(day_number);
            let interval_complete = self.current_volume_day == Some(day_number)
                && self.current_interval_bars == 30;
            let interval_volume = self.current_interval_volume;
            let historical_volume_ok = interval_complete
                && self.volume_exceeds_previous_median(interval_volume);
            let reference_high = self.expiry_high;

            if interval_complete {
                self.record_friday_volume(interval_volume);
            }

            if let Some(high) = reference_high {
                let price_breakout =
                    i128::from(bar.close.raw()) * 10_000 >= i128::from(high.raw()) * 10_015;
                if historical_volume_ok
                    && price_breakout
                    && ctx.open_orders() == 0
                    && ctx.position(INSTRUMENT).is_zero()
                {
                    let half_equity = ctx.equity().mul_ratio(1, 2);
                    let cash = ctx.cash();
                    let budget = if half_equity < cash { half_equity } else { cash };
                    if budget.is_positive() {
                        let affordable = ctx.affordable_qty(
                            INSTRUMENT,
                            bar.close,
                            budget,
                            SIZING_MARGIN_PPM,
                        );
                        let volume_cap = bar.volume.mul_ratio(1, 10);
                        let qty = Qty::from_raw(affordable.raw().min(volume_cap.raw()));
                        if qty.is_positive() {
                            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                        }
                    }
                }
            }
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() && ctx.open_orders() == 0 {
            let time_exit = self
                .entry_ts_ns
                .map(|entry| {
                    now_ns >= entry.saturating_add(TimeDelta::from_hours(12).as_nanos())
                })
                .unwrap_or(false);
            let thirty_minute_close = minute_of_day.rem_euclid(30) == 0;
            let level_exit = thirty_minute_close
                && self
                    .expiry_high
                    .map(|high| bar.close < high)
                    .unwrap_or(false);

            if time_exit || level_exit {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        match fill.side {
            Side::Buy => {
                if self.entry_ts_ns.is_none() {
                    self.entry_ts_ns = Some(fill.ts.as_nanos());
                }
            }
            Side::Sell => {
                if ctx.position(INSTRUMENT).is_zero() {
                    self.entry_ts_ns = None;
                }
            }
        }
    }
}

export_strategy!(WeeklyExpiryHedgeUnwind::new());
