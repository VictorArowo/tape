use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, Money, OrderRequest, Price, Side, Strategy, TimeDelta,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const DAILY_SMA_LEN: usize = 100;
const FOUR_HOUR_SMA_LEN: usize = 5;
const RSI_THRESHOLD: f64 = 5.0;
const MAX_HOLD_FOUR_HOUR_BARS: u64 = 120;
const SIZING_MARGIN_PPM: i64 = 0;

struct PeriodAggregator {
    period_ns: i64,
    bucket: Option<i64>,
    close: Option<Price>,
}

impl PeriodAggregator {
    fn new(period_ns: i64) -> Self {
        Self {
            period_ns,
            bucket: None,
            close: None,
        }
    }

    fn push(&mut self, bar: &Bar) -> Vec<Price> {
        let mut completed = Vec::new();
        let bucket = bar.open_ts.as_nanos().div_euclid(self.period_ns);

        if self.bucket != Some(bucket) {
            if let Some(close) = self.close.take() {
                completed.push(close);
            }
            self.bucket = Some(bucket);
        }

        self.close = Some(bar.close);
        if bar.close_ts.as_nanos().rem_euclid(self.period_ns) == 0 {
            if let Some(close) = self.close.take() {
                completed.push(close);
            }
            self.bucket = None;
        }

        completed
    }
}

struct CloseSma {
    len: usize,
    closes: VecDeque<i64>,
    sum: i128,
}

impl CloseSma {
    fn new(len: usize) -> Self {
        Self {
            len,
            closes: VecDeque::with_capacity(len + 1),
            sum: 0,
        }
    }

    fn push(&mut self, close: Price) {
        self.closes.push_back(close.raw());
        self.sum += i128::from(close.raw());
        if self.closes.len() > self.len {
            if let Some(old) = self.closes.pop_front() {
                self.sum -= i128::from(old);
            }
        }
    }

    fn ready_sum(&self) -> Option<i128> {
        (self.closes.len() == self.len).then_some(self.sum)
    }
}

struct WilderRsi2 {
    previous_close: Option<f64>,
    seed_gain: f64,
    seed_loss: f64,
    seed_changes: usize,
    average_gain: Option<f64>,
    average_loss: Option<f64>,
    previous_rsi: Option<f64>,
}

impl WilderRsi2 {
    fn new() -> Self {
        Self {
            previous_close: None,
            seed_gain: 0.0,
            seed_loss: 0.0,
            seed_changes: 0,
            average_gain: None,
            average_loss: None,
            previous_rsi: None,
        }
    }

    fn push(&mut self, close: Price) -> bool {
        let close = close.to_f64();
        let Some(previous_close) = self.previous_close.replace(close) else {
            return false;
        };

        let change = close - previous_close;
        let gain = change.max(0.0);
        let loss = (-change).max(0.0);

        if let (Some(average_gain), Some(average_loss)) =
            (self.average_gain.as_mut(), self.average_loss.as_mut())
        {
            *average_gain = (*average_gain + gain) / 2.0;
            *average_loss = (*average_loss + loss) / 2.0;
        } else {
            self.seed_gain += gain;
            self.seed_loss += loss;
            self.seed_changes += 1;
            if self.seed_changes < 2 {
                return false;
            }
            self.average_gain = Some(self.seed_gain / 2.0);
            self.average_loss = Some(self.seed_loss / 2.0);
        }

        let average_gain = self.average_gain.unwrap_or(0.0);
        let average_loss = self.average_loss.unwrap_or(0.0);
        let rsi = if average_loss == 0.0 {
            100.0
        } else {
            100.0 - 100.0 / (1.0 + average_gain / average_loss)
        };
        let crossed_down = self
            .previous_rsi
            .is_some_and(|previous| previous >= RSI_THRESHOLD && rsi < RSI_THRESHOLD);
        self.previous_rsi = Some(rsi);
        crossed_down
    }
}

struct BullRegimeRsiDip {
    daily_aggregator: PeriodAggregator,
    four_hour_aggregator: PeriodAggregator,
    daily_sma: CloseSma,
    latest_daily_close: Option<Price>,
    daily_regime: bool,
    four_hour_sma: CloseSma,
    rsi: WilderRsi2,
    completed_four_hour_bars: u64,
    entry_four_hour_count: Option<u64>,
    exit_active: bool,
}

impl BullRegimeRsiDip {
    fn new() -> Self {
        Self {
            daily_aggregator: PeriodAggregator::new(TimeDelta::from_days(1).as_nanos()),
            four_hour_aggregator: PeriodAggregator::new(TimeDelta::from_hours(4).as_nanos()),
            daily_sma: CloseSma::new(DAILY_SMA_LEN),
            latest_daily_close: None,
            daily_regime: false,
            four_hour_sma: CloseSma::new(FOUR_HOUR_SMA_LEN),
            rsi: WilderRsi2::new(),
            completed_four_hour_bars: 0,
            entry_four_hour_count: None,
            exit_active: false,
        }
    }
}

impl Strategy for BullRegimeRsiDip {
    fn name(&self) -> String {
        String::from("Bull-Regime RSI Dip")
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let completed_daily = self.daily_aggregator.push(bar);
        let completed_four_hour = self.four_hour_aggregator.push(bar);

        for close in completed_daily {
            self.daily_sma.push(close);
            self.latest_daily_close = Some(close);
            self.daily_regime = match self.daily_sma.ready_sum() {
                Some(sum) => i128::from(close.raw()) * DAILY_SMA_LEN as i128 > sum,
                None => false,
            };
        }

        let mut crossed_down = false;
        for close in completed_four_hour {
            self.completed_four_hour_bars = self.completed_four_hour_bars.saturating_add(1);
            crossed_down |= self.rsi.push(close);
            self.four_hour_sma.push(close);

            let above_four_hour_sma = self
                .four_hour_sma
                .ready_sum()
                .is_some_and(|sum| {
                    i128::from(close.raw()) * FOUR_HOUR_SMA_LEN as i128 > sum
                });
            let held_long_enough = self.entry_four_hour_count.is_some_and(|entry_count| {
                self.completed_four_hour_bars.saturating_sub(entry_count)
                    >= MAX_HOLD_FOUR_HOUR_BARS
            });

            if ctx.position(INSTRUMENT).is_positive()
                && (above_four_hour_sma || !self.daily_regime || held_long_enough)
            {
                self.exit_active = true;
            }
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() && !self.daily_regime {
            self.exit_active = true;
        }

        let was_exiting = self.exit_active;
        if self.exit_active {
            if position.is_zero() {
                if ctx.open_orders() == 0 {
                    self.exit_active = false;
                    self.entry_four_hour_count = None;
                } else {
                    return;
                }
            } else {
                if ctx.open_orders() == 0 {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                }
                return;
            }
        }

        if was_exiting || !position.is_zero() || ctx.open_orders() != 0 {
            return;
        }
        if !crossed_down || !self.daily_regime || bar.close.is_zero() {
            return;
        }

        let equity_budget = ctx.equity().mul_ratio(99, 100);
        let cash = ctx.cash();
        let budget = Money::from_raw(equity_budget.raw().min(cash.raw()));
        if !budget.is_positive() {
            return;
        }

        let qty = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            budget,
            SIZING_MARGIN_PPM,
        );
        if qty.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.side == Side::Buy && self.entry_four_hour_count.is_none() {
            self.entry_four_hour_count = Some(self.completed_four_hour_bars);
        }
    }
}

export_strategy!(BullRegimeRsiDip::new());
