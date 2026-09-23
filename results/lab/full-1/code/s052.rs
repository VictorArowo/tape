use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const RETURN_WINDOW: usize = 168;
const CLOSE_WINDOW: usize = 25;
const ENTRY_THRESHOLD: f64 = 0.10;
const SIZING_MARGIN_PPM: i64 = 5_000;
const SCALE: i128 = 100_000_000;

struct AutocorrelationStateFilter {
    hourly_returns: VecDeque<f64>,
    hourly_closes: VecDeque<i64>,
}

impl AutocorrelationStateFilter {
    fn new() -> Self {
        Self {
            hourly_returns: VecDeque::with_capacity(RETURN_WINDOW + 1),
            hourly_closes: VecDeque::with_capacity(CLOSE_WINDOW + 1),
        }
    }

    fn push_hourly_close(&mut self, close_raw: i64) {
        if close_raw <= 0 {
            return;
        }

        if let Some(&previous_raw) = self.hourly_closes.back() {
            if previous_raw > 0 {
                let ret = (close_raw as f64 / previous_raw as f64).ln();
                if ret.is_finite() {
                    self.hourly_returns.push_back(ret);
                    if self.hourly_returns.len() > RETURN_WINDOW {
                        self.hourly_returns.pop_front();
                    }
                }
            }
        }

        self.hourly_closes.push_back(close_raw);
        if self.hourly_closes.len() > CLOSE_WINDOW {
            self.hourly_closes.pop_front();
        }
    }

    fn lag1_pearson(&self) -> Option<f64> {
        if self.hourly_returns.len() < RETURN_WINDOW {
            return None;
        }

        let returns: Vec<f64> = self.hourly_returns.iter().copied().collect();
        let pairs = returns.len() - 1;
        if pairs == 0 {
            return None;
        }

        let mean_x = returns[..pairs].iter().sum::<f64>() / pairs as f64;
        let mean_y = returns[1..].iter().sum::<f64>() / pairs as f64;
        let mut covariance = 0.0;
        let mut variance_x = 0.0;
        let mut variance_y = 0.0;

        for i in 0..pairs {
            let x = returns[i] - mean_x;
            let y = returns[i + 1] - mean_y;
            covariance += x * y;
            variance_x += x * x;
            variance_y += y * y;
        }

        let denominator = (variance_x * variance_y).sqrt();
        if denominator <= 0.0 || !denominator.is_finite() {
            return Some(0.0);
        }

        let correlation = covariance / denominator;
        correlation.is_finite().then_some(correlation.clamp(-1.0, 1.0))
    }

    fn target_qty(equity_raw: i64, price_raw: i64) -> Qty {
        if equity_raw <= 0 || price_raw <= 0 {
            return Qty::from_raw(0);
        }

        let raw = (i128::from(equity_raw) * SCALE) / i128::from(price_raw);
        let raw = raw.clamp(0, i128::from(i64::MAX)) as i64;
        Qty::from_raw(raw)
    }
}

impl Strategy for AutocorrelationStateFilter {
    fn name(&self) -> String {
        "autocorrelation-state-filter".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let hour_ns = TimeDelta::from_hours(1).as_nanos();
        if hour_ns <= 0 || bar.close_ts.as_nanos().rem_euclid(hour_ns) != 0 {
            return;
        }

        self.push_hourly_close(bar.close.raw());

        if ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        let ready = self.hourly_returns.len() >= RETURN_WINDOW
            && self.hourly_closes.len() >= CLOSE_WINDOW;

        if !ready {
            if position.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
            return;
        }

        let Some(autocorrelation) = self.lag1_pearson() else {
            return;
        };
        let oldest_24h_close = self.hourly_closes[0];
        let latest_close = *self.hourly_closes.back().expect("ready close window");
        let positive_24h_return = latest_close > oldest_24h_close;

        if position.is_positive() {
            if autocorrelation < 0.0 || !positive_24h_return {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
        } else if position.is_zero()
            && autocorrelation > ENTRY_THRESHOLD
            && positive_24h_return
        {
            let target = Self::target_qty(ctx.equity().raw(), bar.close.raw());
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            let qty = Qty::from_raw(target.raw().min(affordable.raw()));
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        }
    }
}

export_strategy!(AutocorrelationStateFilter::new());
