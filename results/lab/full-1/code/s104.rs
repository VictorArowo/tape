use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const HOURLY_CLOSES: usize = 72;
const SIZING_MARGIN_PPM: i64 = 5_000;
const ENTRY_DOWNSIDE_RATIO: f64 = 0.70;
const EXIT_DOWNSIDE_RATIO: f64 = 1.30;

struct SemivarianceAsymmetry {
    hourly_closes: VecDeque<i64>,
    last_hour: Option<i64>,
    has_signal: bool,
    target_long: bool,
}

impl SemivarianceAsymmetry {
    fn semivariances(&self) -> Option<(f64, f64)> {
        if self.hourly_closes.len() != HOURLY_CLOSES {
            return None;
        }

        let mut upside = 0.0;
        let mut downside = 0.0;
        for i in 1..self.hourly_closes.len() {
            let previous = *self.hourly_closes.get(i - 1)?;
            let current = *self.hourly_closes.get(i)?;
            if previous <= 0 || current <= 0 {
                return None;
            }

            let ret = (current as f64 / previous as f64).ln();
            if ret > 0.0 {
                upside += ret * ret;
            } else if ret < 0.0 {
                downside += ret * ret;
            }
        }
        Some((upside, downside))
    }
}

impl Strategy for SemivarianceAsymmetry {
    fn name(&self) -> String {
        "Semivariance Asymmetry".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let close_minutes = bar.close_ts.as_nanos().div_euclid(minute_ns);
        let minute_of_day = close_minutes.rem_euclid(1440);
        if minute_of_day % 60 == 0 {
            let hour = close_minutes.div_euclid(60);
            if self.last_hour != Some(hour) {
                self.last_hour = Some(hour);
                self.hourly_closes.push_back(bar.close.raw());
                if self.hourly_closes.len() > HOURLY_CLOSES {
                    self.hourly_closes.pop_front();
                }

                if let Some((upside, downside)) = self.semivariances() {
                    let position = ctx.position(INSTRUMENT);
                    self.target_long = if position.is_zero() {
                        upside > 0.0 && downside <= ENTRY_DOWNSIDE_RATIO * upside
                    } else {
                        downside < EXIT_DOWNSIDE_RATIO * upside
                    };
                    self.has_signal = true;
                }
            }
        }

        if !self.has_signal || ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if self.target_long {
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        } else if position.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
        }
    }

    fn on_finish(&mut self, ctx: &mut Ctx) {
        let position = ctx.position(INSTRUMENT);
        if ctx.open_orders() == 0 && position.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
        }
    }
}

export_strategy!(SemivarianceAsymmetry {
    hourly_closes: VecDeque::with_capacity(HOURLY_CLOSES + 1),
    last_hour: None,
    has_signal: false,
    target_long: false,
});
