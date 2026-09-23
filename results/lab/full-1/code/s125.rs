use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const VWAP_WINDOW: usize = 1_440;
const SIGMA_WINDOW: usize = 10_080;
const MAX_VOLUME_FRACTION_NUM: i64 = 1;
const MAX_VOLUME_FRACTION_DEN: i64 = 10;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct RollingVwapReversion {
    weighted_prices: VecDeque<(f64, f64)>,
    weighted_price_sum: f64,
    volume_sum: f64,
    log_deviations: VecDeque<f64>,
    log_sum: f64,
    log_sum_sq: f64,
    previous_close: Option<f64>,
    previous_lower_band: Option<f64>,
    entry_ts: Option<i64>,
    exit_latched: bool,
    cycle_bought_raw: i128,
    cycle_sold_raw: i128,
    completed_round_trips: u64,
}

impl RollingVwapReversion {
    fn new() -> Self {
        Self {
            weighted_prices: VecDeque::with_capacity(VWAP_WINDOW + 1),
            weighted_price_sum: 0.0,
            volume_sum: 0.0,
            log_deviations: VecDeque::with_capacity(SIGMA_WINDOW + 1),
            log_sum: 0.0,
            log_sum_sq: 0.0,
            previous_close: None,
            previous_lower_band: None,
            entry_ts: None,
            exit_latched: false,
            cycle_bought_raw: 0,
            cycle_sold_raw: 0,
            completed_round_trips: 0,
        }
    }

    fn push_bar(&mut self, bar: &Bar) -> Option<f64> {
        let volume = bar.volume.to_f64();
        let typical_price = (bar.high.to_f64() + bar.low.to_f64() + bar.close.to_f64()) / 3.0;
        let weighted_price = typical_price * volume;

        self.weighted_prices.push_back((weighted_price, volume));
        self.weighted_price_sum += weighted_price;
        self.volume_sum += volume;
        if self.weighted_prices.len() > VWAP_WINDOW {
            if let Some((old_weighted_price, old_volume)) = self.weighted_prices.pop_front() {
                self.weighted_price_sum -= old_weighted_price;
                self.volume_sum -= old_volume;
            }
        }

        (self.weighted_prices.len() == VWAP_WINDOW && self.volume_sum > 0.0)
            .then(|| self.weighted_price_sum / self.volume_sum)
    }

    fn push_log_deviation(&mut self, value: f64) -> Option<f64> {
        self.log_deviations.push_back(value);
        self.log_sum += value;
        self.log_sum_sq += value * value;
        if self.log_deviations.len() > SIGMA_WINDOW {
            if let Some(old_value) = self.log_deviations.pop_front() {
                self.log_sum -= old_value;
                self.log_sum_sq -= old_value * old_value;
            }
        }

        if self.log_deviations.len() != SIGMA_WINDOW {
            return None;
        }

        let count = SIGMA_WINDOW as f64;
        let mean = self.log_sum / count;
        let variance = (self.log_sum_sq / count - mean * mean).max(0.0);
        Some(variance.sqrt())
    }

    fn volume_capped_qty(&self, requested: Qty, bar: &Bar) -> Qty {
        let cap = bar
            .volume
            .mul_ratio(MAX_VOLUME_FRACTION_NUM, MAX_VOLUME_FRACTION_DEN);
        Qty::from_raw(requested.raw().min(cap.raw()))
    }
}

impl Strategy for RollingVwapReversion {
    fn name(&self) -> String {
        "Rolling VWAP Reversion".to_string()
    }

    fn on_start(&mut self, ctx: &mut Ctx) {
        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            self.entry_ts = Some(ctx.now().as_nanos());
            self.cycle_bought_raw = i128::from(position.raw());
        }
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let close = bar.close.to_f64();
        let vwap = self.push_bar(bar);
        let sigma = vwap.and_then(|value| {
            (close > 0.0 && value > 0.0)
                .then(|| self.push_log_deviation((close / value).ln()))
                .flatten()
        });

        let lower_1_5 = vwap.zip(sigma).map(|(value, deviation)| {
            value * (-1.5 * deviation).exp()
        });
        let lower_3 = vwap.zip(sigma).map(|(value, deviation)| {
            value * (-3.0 * deviation).exp()
        });

        let crossed_up = self
            .previous_close
            .zip(self.previous_lower_band)
            .zip(lower_1_5)
            .is_some_and(|((prior_close, prior_band), current_band)| {
                prior_close <= prior_band && close > current_band
            });
        let entry_signal = vwap.zip(lower_1_5).is_some_and(|(value, _)| {
            crossed_up && close <= value * 0.995
        });

        let position = ctx.position(INSTRUMENT);
        let was_exiting = self.exit_latched;
        if position.is_positive() {
            let time_exit = self.entry_ts.is_some_and(|entry_ts| {
                bar.close_ts.as_nanos().saturating_sub(entry_ts)
                    >= TimeDelta::from_hours(24).as_nanos()
            });
            let price_exit = vwap.is_some_and(|value| close >= value);
            let stop_exit = lower_3.is_some_and(|band| close <= band);
            if time_exit || price_exit || stop_exit {
                self.exit_latched = true;
            }
        } else {
            self.exit_latched = false;
        }

        self.previous_close = Some(close);
        self.previous_lower_band = lower_1_5;

        if ctx.open_orders() != 0 {
            return;
        }

        if position.is_positive() {
            if self.exit_latched {
                let qty = self.volume_capped_qty(position, bar);
                if qty.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
                }
            }
            return;
        }

        // Do not re-enter on the same bar that observes a completed exit.
        if was_exiting {
            return;
        }

        if entry_signal {
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            let qty = self.volume_capped_qty(affordable, bar);
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        match fill.side {
            Side::Buy => {
                if self.cycle_bought_raw == 0 {
                    self.entry_ts = Some(fill.ts.as_nanos());
                    self.cycle_sold_raw = 0;
                }
                self.cycle_bought_raw += i128::from(fill.qty.raw());
            }
            Side::Sell => {
                self.cycle_sold_raw += i128::from(fill.qty.raw());
                if self.cycle_bought_raw > 0
                    && self.cycle_sold_raw >= self.cycle_bought_raw
                {
                    self.completed_round_trips += 1;
                    self.cycle_bought_raw = 0;
                    self.cycle_sold_raw = 0;
                    self.entry_ts = None;
                    self.exit_latched = true;
                }
            }
        }
    }

    fn on_finish(&mut self, ctx: &mut Ctx) {
        if self.completed_round_trips < 30 {
            ctx.log(&format!(
                "Rolling VWAP Reversion: {} completed round trips; fewer than 30, so the test is not informative.",
                self.completed_round_trips
            ));
        } else {
            ctx.log(&format!(
                "Rolling VWAP Reversion: {} completed round trips; compare net results out of sample with buy-and-hold.",
                self.completed_round_trips
            ));
        }
    }
}

export_strategy!(RollingVwapReversion::new());
