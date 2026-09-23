use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const LOOKBACK_RETURNS: usize = 72;
const BUY_THRESHOLD: f64 = 0.52;
const SELL_THRESHOLD: f64 = 0.68;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct DownsideVarianceBalance {
    hourly_closes: VecDeque<i64>,
    last_completed_hour: Option<i64>,
}

impl DownsideVarianceBalance {
    fn new() -> Self {
        Self {
            hourly_closes: VecDeque::with_capacity(LOOKBACK_RETURNS + 1),
            last_completed_hour: None,
        }
    }

    fn downside_variance_share(&self) -> Option<f64> {
        if self.hourly_closes.len() < LOOKBACK_RETURNS + 1 {
            return None;
        }

        let mut downside_variance = 0.0_f64;
        let mut total_variance = 0.0_f64;
        for pair in self.hourly_closes.as_slices().0.windows(2) {
            let previous = pair[0] as f64;
            let current = pair[1] as f64;
            if previous <= 0.0 {
                return None;
            }
            let ret = (current - previous) / previous;
            let variance = ret * ret;
            total_variance += variance;
            if ret < 0.0 {
                downside_variance += variance;
            }
        }

        // VecDeque can wrap into two slices; include the pair crossing the boundary
        // and all pairs in the second slice when necessary.
        let (first, second) = self.hourly_closes.as_slices();
        if !second.is_empty() {
            let mut previous = *first.last()? as f64;
            for &raw in second {
                let current = raw as f64;
                if previous <= 0.0 {
                    return None;
                }
                let ret = (current - previous) / previous;
                let variance = ret * ret;
                total_variance += variance;
                if ret < 0.0 {
                    downside_variance += variance;
                }
                previous = current;
            }
        }

        (total_variance > 0.0).then_some(downside_variance / total_variance)
    }

    fn record_hourly_close(&mut self, hour: i64, close_raw: i64) -> bool {
        if self.last_completed_hour == Some(hour) {
            return false;
        }
        self.last_completed_hour = Some(hour);
        self.hourly_closes.push_back(close_raw);
        while self.hourly_closes.len() > LOOKBACK_RETURNS + 1 {
            self.hourly_closes.pop_front();
        }
        true
    }
}

impl Strategy for DownsideVarianceBalance {
    fn name(&self) -> String {
        "downside-variance-balance".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        // A one-minute bar closing exactly on the UTC hour boundary supplies
        // that completed hour's aggregate close.
        let hour_ns = TimeDelta::from_hours(1).as_nanos();
        let close_ns = bar.close_ts.as_nanos();
        if close_ns.rem_euclid(hour_ns) != 0 {
            return;
        }
        let completed_hour = close_ns.div_euclid(hour_ns);
        if !self.record_hourly_close(completed_hour, bar.close.raw()) {
            return;
        }

        let Some(share) = self.downside_variance_share() else {
            return;
        };
        if ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_zero() && share < BUY_THRESHOLD {
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        } else if position.is_positive() && share > SELL_THRESHOLD {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
        }
    }
}

export_strategy!(DownsideVarianceBalance::new());
