use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SMA_DAYS: usize = 100;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct DailyTrendFilter {
    current_day: Option<i64>,
    current_close: Option<Price>,
    closes: VecDeque<i64>,
    close_sum: i128,
    target: Option<Qty>,
}

impl DailyTrendFilter {
    fn new() -> Self {
        Self {
            current_day: None,
            current_close: None,
            closes: VecDeque::with_capacity(SMA_DAYS + 1),
            close_sum: 0,
            target: None,
        }
    }

    fn record_completed_day(&mut self, close: Price, ctx: &mut Ctx) {
        self.closes.push_back(close.raw());
        self.close_sum += i128::from(close.raw());
        if self.closes.len() > SMA_DAYS {
            if let Some(old) = self.closes.pop_front() {
                self.close_sum -= i128::from(old);
            }
        }
        if self.closes.len() != SMA_DAYS {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        let close_scaled = i128::from(close.raw()) * SMA_DAYS as i128;
        let above = close_scaled > self.close_sum;
        let below = close_scaled < self.close_sum;

        if position.is_zero() && above {
            let equity_raw = ctx.equity().raw().max(0);
            let price_raw = close.raw();
            if price_raw > 0 {
                let raw_qty = ((i128::from(equity_raw) * 100_000_000_i128)
                    / i128::from(price_raw))
                    .clamp(0, i128::from(i64::MAX)) as i64;
                self.target = Some(Qty::from_raw(raw_qty));
            }
        } else if position.is_positive() && below {
            self.target = Some(Qty::from_raw(0));
        }
    }

    fn work_toward_target(&self, ctx: &mut Ctx, price: Price) {
        if ctx.open_orders() != 0 {
            return;
        }
        let Some(target) = self.target else {
            return;
        };
        let position = ctx.position(INSTRUMENT);
        if target > position {
            let remaining = Qty::from_raw(target.raw().saturating_sub(position.raw()));
            let affordable = ctx.affordable_qty(INSTRUMENT, price, ctx.cash(), SIZING_MARGIN_PPM);
            let qty = if remaining < affordable { remaining } else { affordable };
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        } else if position > target {
            let remaining = Qty::from_raw(position.raw().saturating_sub(target.raw()));
            let qty = if remaining < position { remaining } else { position };
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
            }
        }
    }
}

impl Strategy for DailyTrendFilter {
    fn name(&self) -> String {
        "daily-trend-filter(100)".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }
        let day_nanos = TimeDelta::from_days(1).as_nanos();
        if day_nanos <= 0 {
            return;
        }
        let day = bar.close_ts.as_nanos().div_euclid(day_nanos);
        match self.current_day {
            None => {
                self.current_day = Some(day);
                self.current_close = Some(bar.close);
            }
            Some(previous_day) if day != previous_day => {
                if let Some(close) = self.current_close {
                    self.record_completed_day(close, ctx);
                }
                self.current_day = Some(day);
                self.current_close = Some(bar.close);
            }
            _ => {
                self.current_close = Some(bar.close);
            }
        }
        self.work_toward_target(ctx, bar.close);
    }
}

export_strategy!(DailyTrendFilter::new());