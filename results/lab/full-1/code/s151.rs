use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const HISTORY_CLOSES: usize = 73;
const EXIT_Q: f64 = 0.65;
const ENTRY_Q: f64 = 0.45;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct HourlyCloses {
    bucket: Option<i64>,
    last_close: Option<f64>,
    finalized: bool,
}

impl HourlyCloses {
    fn new() -> Self {
        Self {
            bucket: None,
            last_close: None,
            finalized: false,
        }
    }

    // Bars are assigned to the UTC hour in which they open. A close exactly
    // on an hour boundary completes that hour without waiting for another bar.
    fn observe(&mut self, bar: &Bar) -> Option<f64> {
        let hour_ns = TimeDelta::from_hours(1).as_nanos();
        let open_ns = bar.open_ts.as_nanos();
        let bucket = open_ns.div_euclid(hour_ns);
        let close = bar.close.to_f64();
        let mut completed = None;

        match self.bucket {
            None => {
                self.bucket = Some(bucket);
                self.last_close = Some(close);
                self.finalized = false;
            }
            Some(current) if bucket > current => {
                if !self.finalized {
                    completed = self.last_close;
                }
                self.bucket = Some(bucket);
                self.last_close = Some(close);
                self.finalized = false;
            }
            Some(current) if bucket < current => return None,
            Some(_) => {
                if !self.finalized {
                    self.last_close = Some(close);
                }
            }
        }

        let close_ns = bar.close_ts.as_nanos();
        if !self.finalized
            && close_ns.rem_euclid(hour_ns) == 0
            && close_ns.div_euclid(hour_ns) > bucket
        {
            completed = self.last_close;
            self.finalized = true;
        }

        completed
    }
}

struct DownsideVarianceHysteresis {
    hourly: HourlyCloses,
    closes: VecDeque<f64>,
}

impl DownsideVarianceHysteresis {
    fn on_hour_close(&mut self, ctx: &mut Ctx, close: f64, sizing_price: tape_guest::Price) {
        if !close.is_finite() || close <= 0.0 {
            return;
        }

        self.closes.push_back(close);
        while self.closes.len() > HISTORY_CLOSES {
            self.closes.pop_front();
        }
        if self.closes.len() < HISTORY_CLOSES {
            return;
        }

        let n = self.closes.len();
        let mut total_sq = 0.0;
        let mut downside_sq = 0.0;
        let mut return_72 = 0.0;

        // Seventy-two hourly returns require seventy-three hourly closes.
        for end in (n - 72)..n {
            let previous = self.closes[end - 1];
            let current = self.closes[end];
            if previous <= 0.0 || current <= 0.0 {
                return;
            }
            let r = (current / previous).ln();
            total_sq += r * r;
            if r < 0.0 {
                downside_sq += r * r;
            }
            return_72 += r;
        }
        if total_sq <= 0.0 {
            return;
        }
        let q = downside_sq / total_sq;

        // The latest 24-hour return spans the last twenty-four hourly returns.
        let return_24 = (self.closes[n - 1] / self.closes[n - 25]).ln();
        if ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() && q >= EXIT_Q && return_72 < 0.0 {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
        } else if position.is_zero() && q <= ENTRY_Q && return_24 > 0.0 {
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                sizing_price,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        }
    }
}

impl Strategy for DownsideVarianceHysteresis {
    fn name(&self) -> String {
        "downside-variance-hysteresis(72h)".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }
        if let Some(hour_close) = self.hourly.observe(bar) {
            self.on_hour_close(ctx, hour_close, bar.close);
        }
    }
}

export_strategy!(DownsideVarianceHysteresis {
    hourly: HourlyCloses::new(),
    closes: VecDeque::with_capacity(HISTORY_CLOSES),
});
