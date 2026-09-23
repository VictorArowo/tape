use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, Fill, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, Timestamp,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SMA_DAYS: usize = 100;
const SMA_LOOKBACK: usize = 20;
const PRIOR_HIGH_DAYS: usize = 5;
const RSI_PERIOD: u8 = 3;
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy, Default)]
struct DailyDecision {
    enter: bool,
    exit: bool,
}

struct WilderRsi {
    previous_close: Option<i64>,
    changes: u8,
    seed_gain: f64,
    seed_loss: f64,
    average_gain: Option<f64>,
    average_loss: Option<f64>,
}

impl WilderRsi {
    fn new() -> Self {
        Self {
            previous_close: None,
            changes: 0,
            seed_gain: 0.0,
            seed_loss: 0.0,
            average_gain: None,
            average_loss: None,
        }
    }

    fn push(&mut self, close: i64) -> Option<f64> {
        if let Some(previous) = self.previous_close {
            let change = close as f64 - previous as f64;
            let gain = change.max(0.0);
            let loss = (-change).max(0.0);

            if let (Some(average_gain), Some(average_loss)) =
                (self.average_gain, self.average_loss)
            {
                let period = f64::from(RSI_PERIOD);
                self.average_gain = Some((average_gain * (period - 1.0) + gain) / period);
                self.average_loss = Some((average_loss * (period - 1.0) + loss) / period);
            } else {
                self.seed_gain += gain;
                self.seed_loss += loss;
                self.changes += 1;
                if self.changes == RSI_PERIOD {
                    let period = f64::from(RSI_PERIOD);
                    self.average_gain = Some(self.seed_gain / period);
                    self.average_loss = Some(self.seed_loss / period);
                }
            }
        }
        self.previous_close = Some(close);

        let (Some(gain), Some(loss)) = (self.average_gain, self.average_loss) else {
            return None;
        };
        if loss == 0.0 {
            Some(100.0)
        } else {
            Some(100.0 - 100.0 / (1.0 + gain / loss))
        }
    }
}

struct DailyCandles {
    day_id: Option<i64>,
    high_raw: i64,
    close_raw: i64,
    finalized: bool,
    closes: VecDeque<i64>,
    close_sum: i128,
    sma_sums: VecDeque<i128>,
    prior_highs: VecDeque<i64>,
    rsi: WilderRsi,
}

impl DailyCandles {
    fn new() -> Self {
        Self {
            day_id: None,
            high_raw: 0,
            close_raw: 0,
            finalized: false,
            closes: VecDeque::with_capacity(SMA_DAYS + 1),
            close_sum: 0,
            sma_sums: VecDeque::with_capacity(SMA_LOOKBACK + 1),
            prior_highs: VecDeque::with_capacity(PRIOR_HIGH_DAYS + 1),
            rsi: WilderRsi::new(),
        }
    }

    fn begin_day(&mut self, day_id: i64, bar: &Bar) {
        self.day_id = Some(day_id);
        self.high_raw = bar.high.raw();
        self.close_raw = bar.close.raw();
        self.finalized = false;
    }

    fn add_bar(&mut self, bar: &Bar) {
        self.high_raw = self.high_raw.max(bar.high.raw());
        self.close_raw = bar.close.raw();
    }

    fn finish_day(&mut self) -> Option<DailyDecision> {
        if self.finalized || self.day_id.is_none() {
            return None;
        }
        self.finalized = true;

        self.closes.push_back(self.close_raw);
        self.close_sum += i128::from(self.close_raw);
        if self.closes.len() > SMA_DAYS {
            self.close_sum -= i128::from(self.closes.pop_front()?);
        }

        let sma_sum = (self.closes.len() == SMA_DAYS).then_some(self.close_sum);
        let rsi = self.rsi.push(self.close_raw);
        let previous_sma = if self.sma_sums.len() >= SMA_LOOKBACK {
            Some(self.sma_sums[self.sma_sums.len() - SMA_LOOKBACK])
        } else {
            None
        };
        let previous_high = (self.prior_highs.len() == PRIOR_HIGH_DAYS)
            .then(|| *self.prior_highs.iter().max().expect("five prior highs"));

        let mut decision = DailyDecision::default();
        if let (Some(sma), Some(old_sma), Some(high), Some(rsi)) =
            (sma_sum, previous_sma, previous_high, rsi)
        {
            let close = i128::from(self.close_raw);
            let trend_up = sma > old_sma;
            let above_sma = close * (SMA_DAYS as i128) > sma;
            let pullback = close * 1_000 <= i128::from(high) * 975;
            decision.enter = above_sma && trend_up && pullback && rsi < 15.0;
            decision.exit = rsi >= 60.0 || close * (SMA_DAYS as i128) < sma;
        } else if let (Some(sma), Some(rsi)) = (sma_sum, rsi) {
            decision.exit =
                rsi >= 60.0 || i128::from(self.close_raw) * (SMA_DAYS as i128) < sma;
        }

        if let Some(sma) = sma_sum {
            self.sma_sums.push_back(sma);
            if self.sma_sums.len() > SMA_LOOKBACK + 1 {
                self.sma_sums.pop_front();
            }
        }

        self.prior_highs.push_back(self.high_raw);
        if self.prior_highs.len() > PRIOR_HIGH_DAYS {
            self.prior_highs.pop_front();
        }

        Some(decision)
    }
}

struct TrendFilteredDipReentry {
    daily: DailyCandles,
    entry_ts: Option<Timestamp>,
}

impl Strategy for TrendFilteredDipReentry {
    fn name(&self) -> String {
        "trend-filtered-dip-reentry".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let day_ns = TimeDelta::from_days(1).as_nanos();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let timestamp_ns = bar.close_ts.as_nanos();
        let day_id = timestamp_ns.div_euclid(day_ns);
        let minute_of_day = timestamp_ns.div_euclid(minute_ns).rem_euclid(1_440);
        let mut completed = None;

        match self.daily.day_id {
            Some(current_day) if current_day != day_id => {
                completed = self.daily.finish_day();
                self.daily.begin_day(day_id, bar);
            }
            None => self.daily.begin_day(day_id, bar),
            _ => {}
        }

        if !self.daily.finalized {
            self.daily.add_bar(bar);
            if minute_of_day == 1_439 {
                completed = self.daily.finish_day();
            }
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_zero() {
            self.entry_ts = None;
        }
        if ctx.open_orders() != 0 {
            return;
        }

        if position.is_positive() {
            let time_exit = self.entry_ts.is_some_and(|entry_ts| {
                let elapsed = i128::from(bar.close_ts.as_nanos())
                    - i128::from(entry_ts.as_nanos());
                elapsed >= i128::from(TimeDelta::from_days(7).as_nanos())
            });
            let signal_exit = completed.is_some_and(|decision| decision.exit);
            if time_exit || signal_exit {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
        } else if position.is_zero() && completed.is_some_and(|decision| decision.enter) {
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

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Buy && self.entry_ts.is_none() {
            self.entry_ts = Some(fill.ts);
        } else if fill.side == Side::Sell && ctx.position(INSTRUMENT).is_zero() {
            self.entry_ts = None;
        }
    }
}

export_strategy!(TrendFilteredDipReentry {
    daily: DailyCandles::new(),
    entry_ts: None,
});
