use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const RETURN_WINDOW: usize = 10_080;
const ANNUAL_MINUTES: f64 = 525_600.0;
const TARGET_RISK: f64 = 0.60;
const MIN_WEIGHT: f64 = 0.25;
const MAX_WEIGHT: f64 = 1.00;
const REBALANCE_THRESHOLD: f64 = 0.03;
const SIZING_MARGIN_PPM: i64 = 5_000;
const SCALE: f64 = 100_000_000.0;

struct ContinuousVolatilityBudget {
    previous_close: Option<f64>,
    returns: VecDeque<f64>,
    last_day: Option<i64>,
    initialized: bool,
}

impl ContinuousVolatilityBudget {
    fn new() -> Self {
        Self {
            previous_close: None,
            returns: VecDeque::with_capacity(RETURN_WINDOW),
            last_day: None,
            initialized: false,
        }
    }

    fn push_close(&mut self, close: f64) {
        if close <= 0.0 || !close.is_finite() {
            self.previous_close = None;
            return;
        }

        if let Some(previous) = self.previous_close {
            if previous > 0.0 {
                let value = (close / previous).ln();
                if value.is_finite() {
                    self.returns.push_back(value);
                    if self.returns.len() > RETURN_WINDOW {
                        self.returns.pop_front();
                    }
                }
            }
        }
        self.previous_close = Some(close);
    }

    fn annualized_volatility(&self) -> Option<f64> {
        if self.returns.len() != RETURN_WINDOW {
            return None;
        }

        // Welford's method computes the population standard deviation of the window.
        let mut count = 0.0;
        let mut mean = 0.0;
        let mut sum_squared_deviations = 0.0;
        for &value in &self.returns {
            count += 1.0;
            let delta = value - mean;
            mean += delta / count;
            sum_squared_deviations += delta * (value - mean);
        }

        let variance = (sum_squared_deviations / count).max(0.0);
        Some(variance.sqrt() * ANNUAL_MINUTES.sqrt())
    }
}

impl Strategy for ContinuousVolatilityBudget {
    fn name(&self) -> String {
        "continuous-volatility-budget".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let close = bar.close.to_f64();
        self.push_close(close);

        let day_ns = TimeDelta::from_days(1).as_nanos();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let timestamp_ns = bar.close_ts.as_nanos();
        let day = timestamp_ns.div_euclid(day_ns);
        let minute_of_day = timestamp_ns.rem_euclid(day_ns) / minute_ns;
        let day_boundary = self
            .last_day
            .map(|previous_day| previous_day != day)
            .unwrap_or(minute_of_day == 0);
        self.last_day = Some(day);

        // Market orders submitted after a bar closes are handled by the sandbox
        // for the next bar's open.
        if !self.initialized {
            self.initialized = true;
            if ctx.open_orders() == 0 {
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
            return;
        }

        if !day_boundary || ctx.open_orders() != 0 || close <= 0.0 {
            return;
        }

        let Some(annualized_volatility) = self.annualized_volatility() else {
            return;
        };
        let target_weight = if annualized_volatility <= 0.0 {
            MAX_WEIGHT
        } else {
            (TARGET_RISK / annualized_volatility).clamp(MIN_WEIGHT, MAX_WEIGHT)
        };

        let equity = ctx.equity().to_f64();
        if equity <= 0.0 || !equity.is_finite() {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        let current_notional = (bar.close * position).to_f64();
        let current_weight = current_notional / equity;
        if !current_weight.is_finite()
            || (target_weight - current_weight).abs() < REBALANCE_THRESHOLD
        {
            return;
        }

        let target_qty_coins = equity * target_weight / close;
        if !target_qty_coins.is_finite() || target_qty_coins <= 0.0 {
            return;
        }
        let target_qty_raw = (target_qty_coins * SCALE) as i64;

        if target_weight > current_weight {
            let needed_raw = target_qty_raw.saturating_sub(position.raw());
            if needed_raw <= 0 {
                return;
            }
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            let qty = Qty::from_raw(needed_raw.min(affordable.raw()));
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        } else if position.is_positive() {
            let sell_raw = position
                .raw()
                .saturating_sub(target_qty_raw)
                .min(position.raw());
            let qty = Qty::from_raw(sell_raw);
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
            }
        }
    }
}

export_strategy!(ContinuousVolatilityBudget::new());
