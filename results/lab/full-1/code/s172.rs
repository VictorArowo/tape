use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta, Timestamp, Fill, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const VOL_WINDOW: usize = 60;
const RETURN_WINDOW: usize = 10;
const MIN_ROUND_TRIPS: u32 = 30;

struct MultiDayDrawdownBounce {
    current_day: Option<i64>,
    current_close: Price,
    current_close_ts: Timestamp,
    previous_daily_day: Option<i64>,
    previous_daily_close: Option<Price>,
    returns: VecDeque<f64>,
    armed: bool,
    entry_price: Option<Price>,
    entry_ts: Option<Timestamp>,
    entry_qty: Qty,
    round_trips: u32,
}

impl MultiDayDrawdownBounce {
    fn utc_day(ts: Timestamp) -> i64 {
        ts.as_nanos().div_euclid(TimeDelta::from_days(1).as_nanos())
    }

    fn z10(&self) -> Option<f64> {
        if self.returns.len() < VOL_WINDOW {
            return None;
        }

        let mean = self.returns.iter().sum::<f64>() / VOL_WINDOW as f64;
        let variance = self
            .returns
            .iter()
            .map(|r| {
                let difference = *r - mean;
                difference * difference
            })
            .sum::<f64>()
            / (VOL_WINDOW - 1) as f64;
        let sigma = variance.sqrt();
        if !sigma.is_finite() || sigma <= 0.0 {
            return None;
        }

        let r10 = self
            .returns
            .iter()
            .skip(VOL_WINDOW - RETURN_WINDOW)
            .sum::<f64>();
        let z = r10 / (sigma * (RETURN_WINDOW as f64).sqrt());
        z.is_finite().then_some(z)
    }

    fn complete_daily_close(&mut self) -> (Price, Timestamp, Option<f64>) {
        let day = self.current_day.unwrap_or(0);
        let close = self.current_close;
        let close_ts = self.current_close_ts;

        let consecutive = self
            .previous_daily_day
            .and_then(|previous_day| previous_day.checked_add(1))
            == Some(day);

        if consecutive {
            if let Some(previous_close) = self.previous_daily_close {
                let previous = previous_close.to_f64();
                let current = close.to_f64();
                if previous > 0.0 && current > 0.0 {
                    self.returns.push_back((current / previous).ln());
                    if self.returns.len() > VOL_WINDOW {
                        self.returns.pop_front();
                    }
                } else {
                    self.returns.clear();
                }
            } else {
                self.returns.clear();
            }
        } else {
            self.returns.clear();
        }

        self.previous_daily_day = Some(day);
        self.previous_daily_close = Some(close);
        (close, close_ts, self.z10())
    }

    fn should_exit(&self, daily_close: Price, close_ts: Timestamp) -> bool {
        let Some(entry_price) = self.entry_price else {
            return false;
        };

        let close_raw = i128::from(daily_close.raw());
        let entry_raw = i128::from(entry_price.raw());
        let take_profit = close_raw * 10_000 >= entry_raw * 10_250;
        let stop_loss = close_raw * 10_000 <= entry_raw * 9_600;
        let time_exit = self.entry_ts.is_some_and(|entry_ts| {
            close_ts
                .as_nanos()
                .saturating_sub(entry_ts.as_nanos())
                >= TimeDelta::from_days(5).as_nanos()
        });

        take_profit || stop_loss || time_exit
    }

    fn evaluate_completed_day(
        &mut self,
        ctx: &mut Ctx,
        bar: &Bar,
        daily_close: Price,
        close_ts: Timestamp,
        z10: Option<f64>,
    ) {
        if ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            if self.should_exit(daily_close, close_ts) {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
            return;
        }
        if !position.is_zero() {
            return;
        }

        // Reconcile local entry state if the position was closed outside a fill callback.
        if self.entry_price.is_some() {
            self.entry_price = None;
            self.entry_ts = None;
            self.entry_qty = Qty::from_raw(0);
        }

        let Some(z) = z10 else {
            return;
        };
        if !self.armed && z > -0.25 {
            self.armed = true;
        }
        if !self.armed || z > -1.0 || !bar.close.is_positive() {
            return;
        }

        let qty = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        if qty.is_positive() {
            self.armed = false;
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }
}

impl Strategy for MultiDayDrawdownBounce {
    fn name(&self) -> String {
        "multi-day-drawdown-bounce".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let day = Self::utc_day(bar.close_ts);
        if let Some(current_day) = self.current_day {
            if day != current_day {
                let (daily_close, close_ts, z10) = self.complete_daily_close();
                self.evaluate_completed_day(ctx, bar, daily_close, close_ts, z10);
            }
        }

        self.current_day = Some(day);
        self.current_close = bar.close;
        self.current_close_ts = bar.close_ts;
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &Fill) {
        match fill.side {
            Side::Buy => {
                if let Some(old_price) = self.entry_price {
                    let old_qty = self.entry_qty.raw();
                    let added_qty = fill.qty.raw();
                    let total_qty = old_qty.saturating_add(added_qty);
                    if total_qty > 0 {
                        let weighted_raw = (i128::from(old_price.raw()) * i128::from(old_qty)
                            + i128::from(fill.price.raw()) * i128::from(added_qty))
                            / i128::from(total_qty);
                        self.entry_price = Some(Price::from_raw(weighted_raw as i64));
                        self.entry_qty = Qty::from_raw(total_qty);
                    }
                } else {
                    self.entry_price = Some(fill.price);
                    self.entry_ts = Some(fill.ts);
                    self.entry_qty = fill.qty;
                }
                self.armed = false;
            }
            Side::Sell => {
                let remaining = self.entry_qty.raw().saturating_sub(fill.qty.raw());
                if remaining <= 0 {
                    if self.entry_price.is_some() {
                        self.round_trips = self.round_trips.saturating_add(1);
                    }
                    self.entry_price = None;
                    self.entry_ts = None;
                    self.entry_qty = Qty::from_raw(0);
                    self.armed = false;
                } else {
                    self.entry_qty = Qty::from_raw(remaining);
                }
            }
        }
    }

    fn on_finish(&mut self, ctx: &mut Ctx) {
        if self.round_trips < MIN_ROUND_TRIPS {
            ctx.log(&format!(
                "REJECT: only {} completed round trips; at least {} are required",
                self.round_trips, MIN_ROUND_TRIPS
            ));
        } else {
            ctx.log(&format!(
                "Completed {} round trips (minimum met)",
                self.round_trips
            ));
        }
    }
}

export_strategy!(MultiDayDrawdownBounce {
    current_day: None,
    current_close: Price::from_raw(0),
    current_close_ts: Timestamp::from_nanos(0),
    previous_daily_day: None,
    previous_daily_close: None,
    returns: VecDeque::with_capacity(VOL_WINDOW + 1),
    armed: true,
    entry_price: None,
    entry_ts: None,
    entry_qty: Qty::from_raw(0),
    round_trips: 0,
});