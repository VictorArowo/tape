use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const HOUR_BARS: usize = 60;
const SIZING_MARGIN_PPM: i64 = 5_000;
const HISTORY_LIMIT: usize = 180;

#[derive(Clone, Copy)]
struct HourBar {
    high: i64,
    low: i64,
    close: i64,
    volume: i128,
}

struct HourAgg {
    key: i64,
    count: usize,
    high: i64,
    low: i64,
    close: i64,
    volume: i128,
}

struct Setup {
    age: usize,
    post_high: i64,
    impulse_volume_sum: i128,
    pullback_volume_sum: i128,
    qualifying_pullback: bool,
}

struct LowVolumePullbackReclaim {
    aggregate: Option<HourAgg>,
    hours: VecDeque<HourBar>,
    ema_seed_sum: f64,
    ema_seed_count: usize,
    ema24: Option<f64>,
    setup: Option<Setup>,
    pending_entry_atr: Option<f64>,
    in_position: bool,
    entry_price: Option<f64>,
    entry_atr: Option<f64>,
    entry_ts: Option<i64>,
    exit_due: bool,
}

impl LowVolumePullbackReclaim {
    fn new() -> Self {
        Self {
            aggregate: None,
            hours: VecDeque::with_capacity(HISTORY_LIMIT),
            ema_seed_sum: 0.0,
            ema_seed_count: 0,
            ema24: None,
            setup: None,
            pending_entry_atr: None,
            in_position: false,
            entry_price: None,
            entry_atr: None,
            entry_ts: None,
            exit_due: false,
        }
    }

    fn update_ema(&mut self, close: f64) -> Option<f64> {
        if let Some(previous) = self.ema24 {
            let alpha = 2.0 / 25.0;
            let value = alpha * close + (1.0 - alpha) * previous;
            self.ema24 = Some(value);
            Some(value)
        } else {
            self.ema_seed_sum += close;
            self.ema_seed_count += 1;
            if self.ema_seed_count == 24 {
                let value = self.ema_seed_sum / 24.0;
                self.ema24 = Some(value);
                Some(value)
            } else {
                None
            }
        }
    }

    fn prior_atr24(&self) -> Option<f64> {
        if self.hours.len() < 24 {
            return None;
        }
        let total: f64 = self
            .hours
            .iter()
            .rev()
            .take(24)
            .map(|hour| (hour.high - hour.low) as f64)
            .sum();
        Some(total / 24.0)
    }

    fn push_history(&mut self, hour: HourBar) {
        self.hours.push_back(hour);
        if self.hours.len() > HISTORY_LIMIT {
            self.hours.pop_front();
        }
    }

    fn submit_exit(&mut self, ctx: &mut Ctx) {
        let position = ctx.position(INSTRUMENT);
        if position.is_positive() && ctx.open_orders() == 0 {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
        }
    }

    fn submit_entry(&mut self, ctx: &mut Ctx, price: Price, atr_raw: f64) {
        if ctx.open_orders() != 0 || !ctx.position(INSTRUMENT).is_zero() {
            return;
        }
        let qty = ctx.affordable_qty(
            INSTRUMENT,
            price,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        if qty.is_positive() {
            self.pending_entry_atr = Some(atr_raw);
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }

    fn finish_hour(&mut self, ctx: &mut Ctx, hour: HourBar, close_ts: i64, signal_price: Price) {
        let prior_atr = self.prior_atr24();
        let close = hour.close as f64;
        let ema = self.update_ema(close);

        if ctx.position(INSTRUMENT).is_positive() || self.in_position {
            if let (Some(entry_price), Some(entry_atr), Some(entry_ts)) =
                (self.entry_price, self.entry_atr, self.entry_ts)
            {
                let price_stop = close <= entry_price - 2.0 * entry_atr;
                let ema_stop = ema.is_some_and(|value| close < value);
                let time_stop = close_ts.saturating_sub(entry_ts)
                    >= TimeDelta::from_hours(48).as_nanos();
                if price_stop || ema_stop || time_stop {
                    self.exit_due = true;
                }
            }
            if self.exit_due {
                self.submit_exit(ctx);
            }
            self.push_history(hour);
            return;
        }

        // A completed exit always requires a fresh impulse; do not create a setup
        // on the same hourly bar that detected or completed that exit.
        if self.exit_due {
            self.exit_due = false;
            self.setup = None;
            self.push_history(hour);
            return;
        }

        if ctx.open_orders() == 0 {
            if let Some(mut setup) = self.setup.take() {
                setup.age += 1;
                setup.post_high = setup.post_high.max(hour.high);
                setup.pullback_volume_sum += hour.volume;

                if (2..=12).contains(&setup.age) {
                    if let Some(atr) = prior_atr {
                        let distance = (setup.post_high - hour.close) as f64;
                        let volume_is_contracting = setup.pullback_volume_sum * 6
                            < setup.impulse_volume_sum * setup.age as i128;
                        if distance >= 0.5 * atr
                            && distance <= 1.5 * atr
                            && volume_is_contracting
                        {
                            setup.qualifying_pullback = true;
                        }
                    }

                    let prior_three_high = if self.hours.len() >= 3 {
                        Some(
                            self.hours
                                .iter()
                                .rev()
                                .take(3)
                                .map(|bar| bar.high)
                                .max()
                                .unwrap_or(i64::MIN),
                        )
                    } else {
                        None
                    };
                    if setup.qualifying_pullback
                        && prior_three_high.is_some_and(|high| hour.close > high)
                    {
                        if let Some(atr) = prior_atr {
                            self.submit_entry(ctx, signal_price, atr);
                        }
                    } else {
                        self.setup = Some(setup);
                    }
                } else if setup.age < 2 {
                    self.setup = Some(setup);
                }
                // Setups expire after their 12-hour pullback/reclaim window.
            } else if let Some(atr) = prior_atr {
                let len = self.hours.len();
                if len >= 168 && len >= 6 {
                    let six_hours_ago = self.hours[len - 6].close as f64;
                    let seven_days_ago = self.hours[len - 168].close;
                    let gain = close - six_hours_ago;
                    let impulse_volume_sum = hour.volume
                        + self
                            .hours
                            .iter()
                            .rev()
                            .take(5)
                            .map(|bar| bar.volume)
                            .sum::<i128>();
                    if gain >= 1.25 * atr && hour.close > seven_days_ago {
                        self.setup = Some(Setup {
                            age: 0,
                            post_high: hour.high,
                            impulse_volume_sum,
                            pullback_volume_sum: 0,
                            qualifying_pullback: false,
                        });
                    }
                }
            }
        }

        self.push_history(hour);
    }
}

impl Strategy for LowVolumePullbackReclaim {
    fn name(&self) -> String {
        "low-volume-pullback-reclaim [wasm]".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        if self.exit_due {
            if ctx.position(INSTRUMENT).is_positive() {
                self.submit_exit(ctx);
            } else if ctx.open_orders() == 0 {
                self.exit_due = false;
            }
        }

        let hour_ns = TimeDelta::from_hours(1).as_nanos();
        let key = bar.open_ts.as_nanos() / hour_ns;

        if self
            .aggregate
            .as_ref()
            .is_some_and(|aggregate| aggregate.key != key)
        {
            if let Some(aggregate) = self.aggregate.take() {
                if aggregate.count >= HOUR_BARS {
                    let completed = HourBar {
                        high: aggregate.high,
                        low: aggregate.low,
                        close: aggregate.close,
                        volume: aggregate.volume,
                    };
                    self.finish_hour(ctx, completed, bar.open_ts.as_nanos(), bar.close);
                }
            }
        }

        if self.aggregate.is_none() {
            self.aggregate = Some(HourAgg {
                key,
                count: 0,
                high: bar.high.raw(),
                low: bar.low.raw(),
                close: bar.close.raw(),
                volume: 0,
            });
        }

        if let Some(aggregate) = self.aggregate.as_mut() {
            aggregate.count += 1;
            aggregate.high = aggregate.high.max(bar.high.raw());
            aggregate.low = aggregate.low.min(bar.low.raw());
            aggregate.close = bar.close.raw();
            aggregate.volume += i128::from(bar.volume.raw());
        }

        if bar.close_ts.as_nanos().rem_euclid(hour_ns) == 0 {
            if let Some(aggregate) = self.aggregate.take() {
                if aggregate.count >= HOUR_BARS {
                    let completed = HourBar {
                        high: aggregate.high,
                        low: aggregate.low,
                        close: aggregate.close,
                        volume: aggregate.volume,
                    };
                    self.finish_hour(ctx, completed, bar.close_ts.as_nanos(), bar.close);
                }
            }
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        match fill.side {
            Side::Buy => {
                if !self.in_position {
                    self.entry_price = Some(fill.price.raw() as f64);
                    self.entry_atr = self.pending_entry_atr.take();
                    self.entry_ts = Some(fill.ts.as_nanos());
                    self.in_position = true;
                    self.exit_due = false;
                }
            }
            Side::Sell => {
                if ctx.position(INSTRUMENT).is_zero() {
                    self.in_position = false;
                    self.entry_price = None;
                    self.entry_atr = None;
                    self.entry_ts = None;
                    self.pending_entry_atr = None;
                    self.setup = None;
                    self.exit_due = false;
                } else {
                    self.exit_due = true;
                }
            }
        }
    }
}

export_strategy!(LowVolumePullbackReclaim::new());
