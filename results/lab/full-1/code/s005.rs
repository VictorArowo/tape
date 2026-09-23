use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const MINUTE_LOOKBACK: usize = 60;
const HOURLY_VOLUME_LOOKBACK: usize = 168;
const HOURLY_TREND_LOOKBACK: usize = 200;
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy)]
struct HourBar {
    close_raw: i64,
    volume_raw: i128,
}

#[derive(Clone, Copy)]
struct ActiveHour {
    key: i64,
    close_raw: i64,
    volume_raw: i128,
}

struct TrendFilteredFlushReversal {
    closes: VecDeque<i64>,
    minute_volumes: VecDeque<i128>,
    minute_volume_sum: i128,
    completed_hours: VecDeque<HourBar>,
    active_hour: Option<ActiveHour>,
    previous_flush_condition: bool,
    entry_price: Option<Price>,
    entry_qty_raw: i128,
    entry_ts_ns: Option<i64>,
}

impl TrendFilteredFlushReversal {
    fn new() -> Self {
        Self {
            closes: VecDeque::with_capacity(MINUTE_LOOKBACK + 1),
            minute_volumes: VecDeque::with_capacity(MINUTE_LOOKBACK),
            minute_volume_sum: 0,
            completed_hours: VecDeque::with_capacity(HOURLY_TREND_LOOKBACK),
            active_hour: None,
            previous_flush_condition: false,
            entry_price: None,
            entry_qty_raw: 0,
            entry_ts_ns: None,
        }
    }

    fn push_completed_hour(&mut self, hour: ActiveHour) {
        self.completed_hours.push_back(HourBar {
            close_raw: hour.close_raw,
            volume_raw: hour.volume_raw,
        });
        if self.completed_hours.len() > HOURLY_TREND_LOOKBACK {
            self.completed_hours.pop_front();
        }
    }

    fn update_hour(&mut self, bar: &Bar, hour_ns: i64) {
        let key = bar.open_ts.as_nanos() / hour_ns;
        let volume = i128::from(bar.volume.raw());
        match self.active_hour {
            Some(mut active) if active.key == key => {
                active.close_raw = bar.close.raw();
                active.volume_raw += volume;
                self.active_hour = Some(active);
            }
            Some(active) => {
                self.push_completed_hour(active);
                self.active_hour = Some(ActiveHour {
                    key,
                    close_raw: bar.close.raw(),
                    volume_raw: volume,
                });
            }
            None => {
                self.active_hour = Some(ActiveHour {
                    key,
                    close_raw: bar.close.raw(),
                    volume_raw: volume,
                });
            }
        }
    }

    fn push_minute(&mut self, bar: &Bar) {
        self.closes.push_back(bar.close.raw());
        if self.closes.len() > MINUTE_LOOKBACK + 1 {
            self.closes.pop_front();
        }

        let volume = i128::from(bar.volume.raw());
        self.minute_volumes.push_back(volume);
        self.minute_volume_sum += volume;
        if self.minute_volumes.len() > MINUTE_LOOKBACK {
            self.minute_volume_sum -= self.minute_volumes.pop_front().unwrap_or(0);
        }
    }

    fn flush_condition(&self) -> Option<bool> {
        if self.closes.len() < MINUTE_LOOKBACK + 1
            || self.minute_volumes.len() < MINUTE_LOOKBACK
            || self.completed_hours.len() < HOURLY_TREND_LOOKBACK
        {
            return None;
        }

        let close_now = i128::from(*self.closes.back()?);
        let close_60_bars_ago = i128::from(*self.closes.front()?);
        let price_flush = close_now * 1_000 <= close_60_bars_ago * 988;

        let first_volume_index = self.completed_hours.len() - HOURLY_VOLUME_LOOKBACK;
        let mut hourly_volumes: Vec<i128> = self
            .completed_hours
            .iter()
            .skip(first_volume_index)
            .map(|hour| hour.volume_raw)
            .collect();
        hourly_volumes.sort_unstable();
        let middle = HOURLY_VOLUME_LOOKBACK / 2;
        let median_twice = hourly_volumes[middle - 1] + hourly_volumes[middle];
        let volume_flush = self.minute_volume_sum * 10 >= median_twice * 9;

        let hourly_close_sum: i128 = self
            .completed_hours
            .iter()
            .map(|hour| i128::from(hour.close_raw))
            .sum();
        let latest_hourly_close = i128::from(self.completed_hours.back()?.close_raw);
        let trend_positive = latest_hourly_close * HOURLY_TREND_LOOKBACK as i128
            > hourly_close_sum;

        Some(price_flush && volume_flush && trend_positive)
    }

    fn record_fill(&mut self, fill_price: Price, qty_raw: i64, ts_ns: i64) {
        if qty_raw <= 0 {
            return;
        }
        if self.entry_qty_raw == 0 {
            self.entry_ts_ns = Some(ts_ns);
            self.entry_price = Some(fill_price);
        } else if let Some(old_price) = self.entry_price {
            let total_notional = i128::from(old_price.raw()) * self.entry_qty_raw
                + i128::from(fill_price.raw()) * i128::from(qty_raw);
            let total_qty = self.entry_qty_raw + i128::from(qty_raw);
            if total_qty > 0 {
                self.entry_price = Some(Price::from_raw((total_notional / total_qty) as i64));
            }
        }
        self.entry_qty_raw += i128::from(qty_raw);
    }
}

impl Strategy for TrendFilteredFlushReversal {
    fn name(&self) -> String {
        "trend-filtered-flush-reversal".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let hour_ns = TimeDelta::from_hours(1).as_nanos();
        self.update_hour(bar, hour_ns);
        self.push_minute(bar);

        let Some(flush_condition) = self.flush_condition() else {
            return;
        };
        let new_flush = flush_condition && !self.previous_flush_condition;
        self.previous_flush_condition = flush_condition;

        if ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            let (Some(entry_price), Some(entry_ts_ns)) = (self.entry_price, self.entry_ts_ns)
            else {
                return;
            };

            let stop_hit = i128::from(bar.low.raw()) * 100
                <= i128::from(entry_price.raw()) * 99;
            let target_hit = i128::from(bar.high.raw()) * 200
                >= i128::from(entry_price.raw()) * 203;
            let time_exit = bar.close_ts.as_nanos().saturating_sub(entry_ts_ns)
                >= TimeDelta::from_hours(6).as_nanos();
            let trend_exit = self.completed_hours.len() >= HOURLY_TREND_LOOKBACK
                && i128::from(self.completed_hours.back().unwrap().close_raw)
                    * HOURLY_TREND_LOOKBACK as i128
                    <= self
                        .completed_hours
                        .iter()
                        .map(|hour| i128::from(hour.close_raw))
                        .sum::<i128>();

            // A stop takes precedence when both price thresholds are touched.
            if stop_hit || target_hit || time_exit || trend_exit {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
        } else if position.is_zero() && new_flush {
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

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        match fill.side {
            Side::Buy => self.record_fill(fill.price, fill.qty.raw(), fill.ts.as_nanos()),
            Side::Sell => {
                self.entry_qty_raw -= i128::from(fill.qty.raw());
                if self.entry_qty_raw <= 0 {
                    self.entry_qty_raw = 0;
                    self.entry_price = None;
                    self.entry_ts_ns = None;
                }
            }
        }
    }
}

export_strategy!(TrendFilteredFlushReversal::new());
