use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, TimeInForce,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const BAR_WINDOW: usize = 60;
const CLOSE_WINDOW: usize = 61;
const HOUR_WINDOW: usize = 24;

#[derive(Clone, Copy)]
struct BarSample {
    high: i64,
    low: i64,
    volume: i128,
}

struct SqueezeContinuation {
    bars: VecDeque<BarSample>,
    closes: VecDeque<i64>,
    completed_hours: VecDeque<i128>,
    active_hour: Option<(i64, i128)>,
    pending_entry: bool,
    entry_ts_ns: Option<i64>,
    last_exit_ts_ns: Option<i64>,
}

impl SqueezeContinuation {
    fn new() -> Self {
        Self {
            bars: VecDeque::with_capacity(BAR_WINDOW + 1),
            closes: VecDeque::with_capacity(CLOSE_WINDOW + 1),
            completed_hours: VecDeque::with_capacity(HOUR_WINDOW + 1),
            active_hour: None,
            pending_entry: false,
            entry_ts_ns: None,
            last_exit_ts_ns: None,
        }
    }

    fn finish_hour(&mut self, volume: i128) {
        self.completed_hours.push_back(volume);
        while self.completed_hours.len() > HOUR_WINDOW {
            self.completed_hours.pop_front();
        }
    }

    fn update_hour(&mut self, bar: &Bar) {
        let hour_ns = TimeDelta::from_hours(1).as_nanos();
        let bucket = bar.open_ts.as_nanos().div_euclid(hour_ns);

        if let Some((active_bucket, volume)) = self.active_hour {
            if active_bucket != bucket {
                self.finish_hour(volume);
                self.active_hour = None;
            }
        }

        let active = self.active_hour.get_or_insert((bucket, 0));
        active.1 += i128::from(bar.volume.raw());

        if bar.close_ts.as_nanos().rem_euclid(hour_ns) == 0 {
            if let Some((_, volume)) = self.active_hour.take() {
                self.finish_hour(volume);
            }
        }
    }

    fn has_signal(&self) -> bool {
        if self.bars.len() != BAR_WINDOW
            || self.closes.len() != CLOSE_WINDOW
            || self.completed_hours.len() != HOUR_WINDOW
        {
            return false;
        }

        let old_close = i128::from(*self.closes.front().expect("window is full"));
        let latest_close = i128::from(*self.closes.back().expect("window is full"));
        if old_close <= 0 || latest_close * 10_000 < old_close * 10_150 {
            return false;
        }

        let mut total_volume = 0_i128;
        let mut highest = i64::MIN;
        let mut lowest = i64::MAX;
        for sample in &self.bars {
            total_volume += sample.volume;
            highest = highest.max(sample.high);
            lowest = lowest.min(sample.low);
        }

        let range = i128::from(highest) - i128::from(lowest);
        if range <= 0
            || (latest_close - i128::from(lowest)) * 4 < range * 3
        {
            return false;
        }

        let mut hourly: Vec<i128> = self.completed_hours.iter().copied().collect();
        hourly.sort_unstable();
        let middle_pair_sum = hourly[HOUR_WINDOW / 2 - 1] + hourly[HOUR_WINDOW / 2];

        // With 24 values, the median is the average of the two middle values.
        // This comparison avoids rounding that average.
        total_volume * 4 >= middle_pair_sum * 3
    }

    fn participation_cap(volume: Qty) -> Qty {
        volume.mul_ratio(1, 10)
    }

    fn within_cooldown(&self, now_ns: i64) -> bool {
        let Some(exit_ns) = self.last_exit_ts_ns else {
            return false;
        };
        let cooldown_ns = TimeDelta::from_mins(60).as_nanos();
        i128::from(now_ns) - i128::from(exit_ns) < i128::from(cooldown_ns)
    }
}

impl Strategy for SqueezeContinuation {
    fn name(&self) -> String {
        "Squeeze Continuation".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        self.update_hour(bar);
        self.bars.push_back(BarSample {
            high: bar.high.raw(),
            low: bar.low.raw(),
            volume: i128::from(bar.volume.raw()),
        });
        while self.bars.len() > BAR_WINDOW {
            self.bars.pop_front();
        }
        self.closes.push_back(bar.close.raw());
        while self.closes.len() > CLOSE_WINDOW {
            self.closes.pop_front();
        }

        let position = ctx.position(INSTRUMENT);

        // Submit an IOC exit once 180 minutes have elapsed since the entry fill.
        // If participation limits the fill, retry on later bars until flat.
        if position.is_positive() {
            self.pending_entry = false;
            if let Some(entry_ns) = self.entry_ts_ns {
                let elapsed_ns = i128::from(bar.close_ts.as_nanos()) - i128::from(entry_ns);
                let exit_delay_ns = i128::from(TimeDelta::from_mins(180).as_nanos());
                if elapsed_ns >= exit_delay_ns && ctx.open_orders() == 0 {
                    let cap = Self::participation_cap(bar.volume);
                    let qty = if cap < position { cap } else { position };
                    if qty.is_positive() {
                        ctx.submit(
                            OrderRequest::market(INSTRUMENT, Side::Sell, qty)
                                .with_time_in_force(TimeInForce::Ioc),
                        );
                    }
                    return;
                }
            }
            return;
        }

        // Use the following completed bar's open and volume for sizing the
        // pending signal's entry. IOC cancels any unfilled remainder.
        if self.pending_entry {
            self.pending_entry = false;
            if ctx.open_orders() == 0
                && !self.within_cooldown(bar.close_ts.as_nanos())
                && bar.open.is_positive()
            {
                let affordable = ctx.affordable_qty(
                    INSTRUMENT,
                    bar.open,
                    ctx.cash(),
                    SIZING_MARGIN_PPM,
                );
                let cap = Self::participation_cap(bar.volume);
                let qty = if cap < affordable { cap } else { affordable };
                if qty.is_positive() {
                    ctx.submit(
                        OrderRequest::market(INSTRUMENT, Side::Buy, qty)
                            .with_time_in_force(TimeInForce::Ioc),
                    );
                }
            }
            return;
        }

        if ctx.open_orders() == 0
            && !self.within_cooldown(bar.close_ts.as_nanos())
            && self.has_signal()
        {
            self.pending_entry = true;
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.side == Side::Buy {
            if self.entry_ts_ns.is_none() {
                self.entry_ts_ns = Some(fill.ts.as_nanos());
            }
        } else if fill.side == Side::Sell && ctx.position(INSTRUMENT).is_zero() {
            self.entry_ts_ns = None;
            self.last_exit_ts_ns = Some(fill.ts.as_nanos());
        }
    }
}

export_strategy!(SqueezeContinuation::new());
