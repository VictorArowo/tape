use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, Fill, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const PRICE_HISTORY_HOURS: i64 = 4;
const COOLDOWN_HOURS: i64 = 8;
const PRIOR_HOURS: usize = 168;

struct HourlyVolumes {
    current_hour: Option<i64>,
    current_volume: i128,
    current_bars: usize,
    completed: VecDeque<(i64, i128)>,
}

impl HourlyVolumes {
    fn new() -> Self {
        Self {
            current_hour: None,
            current_volume: 0,
            current_bars: 0,
            completed: VecDeque::with_capacity(PRIOR_HOURS + 1),
        }
    }

    fn push(&mut self, bar: &Bar) {
        let hour_ns = TimeDelta::from_hours(1).as_nanos();
        let hour = bar.open_ts.as_nanos().div_euclid(hour_ns);
        let volume = i128::from(bar.volume.raw());

        match self.current_hour {
            None => {
                self.current_hour = Some(hour);
                self.current_volume = volume;
                self.current_bars = 1;
            }
            Some(current) if hour == current => {
                self.current_volume += volume;
                self.current_bars += 1;
            }
            Some(current) if hour > current => {
                if self.current_bars == 60 {
                    self.completed.push_back((current, self.current_volume));
                    while self.completed.len() > PRIOR_HOURS + 1 {
                        self.completed.pop_front();
                    }
                }
                self.current_hour = Some(hour);
                self.current_volume = volume;
                self.current_bars = 1;
            }
            Some(_) => {}
        }
    }

    fn latest_is_at_least_one_and_a_half_times_prior_median(&self) -> bool {
        let Some(current_hour) = self.current_hour else {
            return false;
        };
        let needed = PRIOR_HOURS + 1;
        if self.completed.len() < needed {
            return false;
        }

        let start = self.completed.len() - needed;
        let latest_index = self.completed.len() - 1;
        if self.completed[latest_index].0 != current_hour - 1 {
            return false;
        }
        for offset in 0..needed {
            if self.completed[start + offset].0 != self.completed[start].0 + offset as i64 {
                return false;
            }
        }

        let mut prior: Vec<i128> = (0..PRIOR_HOURS)
            .map(|offset| self.completed[start + offset].1)
            .collect();
        prior.sort_unstable();
        let middle = PRIOR_HOURS / 2;
        let latest_volume = self.completed[latest_index].1;
        // For an even-sized median, compare without rounding the middle pair.
        latest_volume * 4 >= (prior[middle - 1] + prior[middle]) * 3
    }
}

struct UpsideSqueezeCooldown {
    closes: VecDeque<(i64, i64)>,
    hourly_volumes: HourlyVolumes,
    exit_fill_ts: Option<i64>,
}

impl UpsideSqueezeCooldown {
    fn new() -> Self {
        Self {
            closes: VecDeque::new(),
            hourly_volumes: HourlyVolumes::new(),
            exit_fill_ts: None,
        }
    }

    fn close_four_hours_ago(&self, now_ns: i64) -> Option<i64> {
        let target = now_ns.saturating_sub(TimeDelta::from_hours(PRICE_HISTORY_HOURS).as_nanos());
        self.closes
            .iter()
            .find_map(|(ts, close)| (*ts == target).then_some(*close))
    }

    fn push_bar_history(&mut self, bar: &Bar) {
        let now_ns = bar.close_ts.as_nanos();
        let four_hours_ns = TimeDelta::from_hours(PRICE_HISTORY_HOURS).as_nanos();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let cutoff = now_ns.saturating_sub(four_hours_ns).saturating_sub(minute_ns);
        while self.closes.front().is_some_and(|(ts, _)| *ts < cutoff) {
            self.closes.pop_front();
        }
        self.closes.push_back((now_ns, bar.close.raw()));
        self.hourly_volumes.push(bar);
    }

    fn squeeze_triggered(&self, bar: &Bar) -> bool {
        let Some(old_close) = self.close_four_hours_ago(bar.close_ts.as_nanos()) else {
            return false;
        };
        let price_up = i128::from(bar.close.raw()) * 100 >= i128::from(old_close) * 102;
        price_up && self.hourly_volumes.latest_is_at_least_one_and_a_half_times_prior_median()
    }
}

impl Strategy for UpsideSqueezeCooldown {
    fn name(&self) -> String {
        "Upside Squeeze Cooldown".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        self.push_bar_history(bar);
        if ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            if self.squeeze_triggered(bar) {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
            return;
        }
        if !position.is_zero() {
            return;
        }

        if let Some(exit_ts) = self.exit_fill_ts {
            let elapsed = ctx.now().as_nanos().saturating_sub(exit_ts);
            if elapsed < TimeDelta::from_hours(COOLDOWN_HOURS).as_nanos() {
                return;
            }
        }

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

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Sell && ctx.position(INSTRUMENT).is_zero() {
            self.exit_fill_ts = Some(fill.ts.as_nanos());
        }
    }
}

export_strategy!(UpsideSqueezeCooldown::new());
