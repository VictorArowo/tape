use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const FIVE_MINUTES: i64 = 5;
const RETURN_WINDOW: usize = 288;
const SIZING_MARGIN_PPM: i64 = 5_000;
const ENTRY_THRESHOLD: f64 = 0.55;
const EXIT_THRESHOLD: f64 = 0.65;

struct DownsideVarianceAsymmetry {
    current_bucket: Option<i64>,
    bucket_count: usize,
    bucket_close: Option<Price>,
    previous_five_minute_close: Option<Price>,
    returns: VecDeque<(f64, f64)>,
    downside_variance: f64,
    upside_variance: f64,
}

impl DownsideVarianceAsymmetry {
    fn new() -> Self {
        Self {
            current_bucket: None,
            bucket_count: 0,
            bucket_close: None,
            previous_five_minute_close: None,
            returns: VecDeque::with_capacity(RETURN_WINDOW + 1),
            downside_variance: 0.0,
            upside_variance: 0.0,
        }
    }

    fn push_return(&mut self, close: Price) {
        if let Some(previous) = self.previous_five_minute_close {
            if previous.is_positive() && close.is_positive() {
                let log_return = (close.to_f64() / previous.to_f64()).ln();
                let (downside, upside) = if log_return < 0.0 {
                    (log_return * log_return, 0.0)
                } else if log_return > 0.0 {
                    (0.0, log_return * log_return)
                } else {
                    (0.0, 0.0)
                };

                self.returns.push_back((downside, upside));
                self.downside_variance += downside;
                self.upside_variance += upside;

                if self.returns.len() > RETURN_WINDOW {
                    if let Some((old_downside, old_upside)) = self.returns.pop_front() {
                        self.downside_variance =
                            (self.downside_variance - old_downside).max(0.0);
                        self.upside_variance = (self.upside_variance - old_upside).max(0.0);
                    }
                }
            }
        }
        self.previous_five_minute_close = Some(close);
    }

    fn on_five_minute_close(&mut self, ctx: &mut Ctx, close: Price) {
        self.push_return(close);
        if self.returns.len() != RETURN_WINDOW {
            return;
        }

        let total_variance = self.downside_variance + self.upside_variance;
        if total_variance <= 0.0 {
            return;
        }
        let asymmetry = self.downside_variance / total_variance;

        if ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if asymmetry <= ENTRY_THRESHOLD && position.is_zero() {
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        } else if asymmetry >= EXIT_THRESHOLD && position.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
        }
    }

    fn on_one_minute_close(&mut self, ctx: &mut Ctx, bar: &Bar) {
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let minute_index = bar.close_ts.as_nanos().div_euclid(minute_ns);

        // A close at minute 5 completes the interval containing closes 1 through 5.
        let interval_index = minute_index - 1;
        let bucket = interval_index.div_euclid(FIVE_MINUTES);
        if self.current_bucket != Some(bucket) {
            self.current_bucket = Some(bucket);
            self.bucket_count = 0;
            self.bucket_close = None;
        }

        self.bucket_count += 1;
        self.bucket_close = Some(bar.close);

        if minute_index.rem_euclid(FIVE_MINUTES) == 0 {
            if self.bucket_count == FIVE_MINUTES as usize {
                if let Some(close) = self.bucket_close {
                    self.on_five_minute_close(ctx, close);
                }
            }
            self.bucket_count = 0;
            self.bucket_close = None;
        }
    }
}

impl Strategy for DownsideVarianceAsymmetry {
    fn name(&self) -> String {
        "downside-variance-asymmetry".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }
        self.on_one_minute_close(ctx, bar);
    }
}

export_strategy!(DownsideVarianceAsymmetry::new());
