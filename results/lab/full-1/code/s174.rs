use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const FOUR_HOURS_MINUTES: i64 = 240;
const RETURN_COUNT: usize = 42;
const PARTICIPATION_NUMERATOR: i64 = 1;
const PARTICIPATION_DENOMINATOR: i64 = 10;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct SerialCorrelationRegime {
    bucket: Option<i64>,
    minutes_in_bucket: usize,
    closes: VecDeque<Price>,
    returns: VecDeque<f64>,
    target_long: Option<bool>,
    entry_remaining: Qty,
    entry_order: Option<OrderId>,
}

impl SerialCorrelationRegime {
    fn new() -> Self {
        Self {
            bucket: None,
            minutes_in_bucket: 0,
            closes: VecDeque::with_capacity(44),
            returns: VecDeque::with_capacity(RETURN_COUNT + 1),
            target_long: None,
            entry_remaining: Qty::from_raw(0),
            entry_order: None,
        }
    }

    fn is_completed_four_hour_bar(&mut self, bar: &Bar) -> bool {
        let period_ns = TimeDelta::from_mins(FOUR_HOURS_MINUTES).as_nanos();
        let bucket = bar.open_ts.as_nanos().div_euclid(period_ns);

        if self.bucket != Some(bucket) {
            self.bucket = Some(bucket);
            self.minutes_in_bucket = 0;
        }
        self.minutes_in_bucket = self.minutes_in_bucket.saturating_add(1);

        bar.close_ts.as_nanos().rem_euclid(period_ns) == 0
            && self.minutes_in_bucket == FOUR_HOURS_MINUTES as usize
    }

    fn push_four_hour_close(&mut self, close: Price) -> Option<(f64, f64)> {
        if !close.is_positive() {
            self.closes.clear();
            self.returns.clear();
            return None;
        }

        if let Some(previous) = self.closes.back().copied() {
            if previous.is_positive() {
                let value = (close.to_f64() / previous.to_f64()).ln();
                if value.is_finite() {
                    self.returns.push_back(value);
                    if self.returns.len() > RETURN_COUNT {
                        self.returns.pop_front();
                    }
                } else {
                    self.closes.clear();
                    self.returns.clear();
                }
            }
        }

        self.closes.push_back(close);
        while self.closes.len() > RETURN_COUNT + 1 {
            self.closes.pop_front();
        }

        if self.returns.len() != RETURN_COUNT || self.closes.len() < 7 {
            return None;
        }

        // The 42 returns provide 41 adjacent pairs for lag-1 Pearson correlation.
        let pair_count = (RETURN_COUNT - 1) as f64;
        let mean_x = (0..RETURN_COUNT - 1)
            .map(|i| self.returns[i])
            .sum::<f64>()
            / pair_count;
        let mean_y = (1..RETURN_COUNT)
            .map(|i| self.returns[i])
            .sum::<f64>()
            / pair_count;

        let mut covariance = 0.0;
        let mut variance_x = 0.0;
        let mut variance_y = 0.0;
        for i in 0..RETURN_COUNT - 1 {
            let x = self.returns[i] - mean_x;
            let y = self.returns[i + 1] - mean_y;
            covariance += x * y;
            variance_x += x * x;
            variance_y += y * y;
        }

        let denominator = (variance_x * variance_y).sqrt();
        let autocorrelation = if denominator > 0.0 && denominator.is_finite() {
            covariance / denominator
        } else {
            0.0
        };

        // The seventh close from the end is six completed 4-hour bars earlier.
        let current_close = self.closes[self.closes.len() - 1].to_f64();
        let prior_close = self.closes[self.closes.len() - 7].to_f64();
        if prior_close <= 0.0 || !current_close.is_finite() || !prior_close.is_finite() {
            return None;
        }
        let return_24h = (current_close / prior_close).ln();
        return_24h
            .is_finite()
            .then_some((autocorrelation, return_24h))
    }

    fn on_signal(&mut self, ctx: &mut Ctx, bar: &Bar, autocorrelation: f64, return_24h: f64) {
        let position = ctx.position(INSTRUMENT);
        let enter_condition = autocorrelation >= 0.10 && return_24h > 0.0;
        let exit_condition = autocorrelation <= 0.0 || return_24h <= 0.0;

        if position.is_zero() {
            if enter_condition {
                if self.target_long != Some(true) {
                    self.entry_remaining = ctx.affordable_qty(
                        INSTRUMENT,
                        bar.close,
                        ctx.cash(),
                        SIZING_MARGIN_PPM,
                    );
                }
                self.target_long = Some(true);
            } else {
                self.target_long = Some(false);
                self.entry_remaining = Qty::from_raw(0);
                self.entry_order = None;
            }
        } else if exit_condition {
            self.target_long = Some(false);
            self.entry_remaining = Qty::from_raw(0);
            self.entry_order = None;
        } else {
            self.target_long = Some(true);
        }
    }

    fn submit_for_next_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if ctx.open_orders() != 0 {
            return;
        }

        let participation_cap = bar
            .volume
            .mul_ratio(PARTICIPATION_NUMERATOR, PARTICIPATION_DENOMINATOR);
        if !participation_cap.is_positive() {
            return;
        }

        match self.target_long {
            Some(true) if self.entry_remaining.is_positive() => {
                let affordable = ctx.affordable_qty(
                    INSTRUMENT,
                    bar.close,
                    ctx.cash(),
                    SIZING_MARGIN_PPM,
                );
                if !affordable.is_positive() {
                    self.entry_remaining = Qty::from_raw(0);
                    return;
                }
                let qty = min_qty(self.entry_remaining, min_qty(affordable, participation_cap));
                if qty.is_positive() {
                    self.entry_order = Some(ctx.submit(OrderRequest::market(
                        INSTRUMENT,
                        Side::Buy,
                        qty,
                    )));
                }
            }
            Some(false) => {
                let position = ctx.position(INSTRUMENT);
                if position.is_positive() {
                    let qty = min_qty(position, participation_cap);
                    if qty.is_positive() {
                        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
                    }
                }
            }
            _ => {}
        }
    }
}

fn min_qty(a: Qty, b: Qty) -> Qty {
    Qty::from_raw(a.raw().min(b.raw()))
}

impl Strategy for SerialCorrelationRegime {
    fn name(&self) -> String {
        "serial-correlation-regime(42,4h)".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        if ctx.open_orders() == 0 {
            self.entry_order = None;
        }

        if self.is_completed_four_hour_bar(bar) {
            if let Some((autocorrelation, return_24h)) = self.push_four_hour_close(bar.close) {
                self.on_signal(ctx, bar, autocorrelation, return_24h);
            }
        }

        self.submit_for_next_bar(ctx, bar);
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if self.entry_order.as_ref() == Some(&fill.order_id) {
            let remaining = self.entry_remaining.raw().saturating_sub(fill.qty.raw()).max(0);
            self.entry_remaining = Qty::from_raw(remaining);
        }
    }
}

export_strategy!(SerialCorrelationRegime::new());
