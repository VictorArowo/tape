use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, Fill, InstrumentId, Money, OrderRequest, Price, Qty, Side, Strategy, TimeDelta,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const SLIPPAGE_NUM: i64 = 10_002;
const SLIPPAGE_DEN: i64 = 10_000;

#[derive(Clone, Copy)]
struct HourCandle {
    hour_id: i64,
    high: Price,
    low: Price,
    close: Price,
}

struct BuildingHour {
    hour_id: i64,
    open: Price,
    high: Price,
    low: Price,
    close: Price,
}

impl BuildingHour {
    fn new(hour_id: i64, bar: &Bar) -> Self {
        Self {
            hour_id,
            open: bar.open,
            high: bar.high,
            low: bar.low,
            close: bar.close,
        }
    }

    fn add(&mut self, bar: &Bar) {
        if bar.high > self.high {
            self.high = bar.high;
        }
        if bar.low < self.low {
            self.low = bar.low;
        }
        self.close = bar.close;
    }

    fn complete(self) -> HourCandle {
        HourCandle {
            hour_id: self.hour_id,
            high: self.high,
            low: self.low,
            close: self.close,
        }
    }
}

struct HourlyChannelBreakout {
    building: Option<BuildingHour>,
    history: VecDeque<HourCandle>,
    entry_remaining: Qty,
    exit_mode: bool,
}

impl HourlyChannelBreakout {
    fn new() -> Self {
        Self {
            building: None,
            history: VecDeque::with_capacity(48),
            entry_remaining: Qty::from_raw(0),
            exit_mode: false,
        }
    }

    fn finish_hour(&mut self, ctx: &mut Ctx, hour: HourCandle) {
        let position = ctx.position(INSTRUMENT);

        if position.is_positive() {
            if self.history.len() >= 24 {
                let previous_low = self
                    .history
                    .iter()
                    .rev()
                    .take(24)
                    .map(|candle| candle.low.raw())
                    .min();
                if previous_low.is_some_and(|low| hour.close.raw() < low) {
                    self.exit_mode = true;
                    self.entry_remaining = Qty::from_raw(0);
                }
            }
        } else if position.is_zero()
            && !self.exit_mode
            && self.entry_remaining.is_zero()
            && self.history.len() >= 48
        {
            let previous_high = self
                .history
                .iter()
                .rev()
                .take(48)
                .map(|candle| candle.high.raw())
                .max();
            let close_24_hours_ago = self
                .history
                .iter()
                .find(|candle| candle.hour_id == hour.hour_id - 24)
                .map(|candle| candle.close);

            if previous_high.is_some_and(|high| hour.close.raw() > high)
                && close_24_hours_ago.is_some_and(|close| hour.close > close)
            {
                let sizing_price = hour.close.mul_ratio(SLIPPAGE_NUM, SLIPPAGE_DEN);
                let sizing_cash = ctx.cash().mul_ratio(95, 100);
                if sizing_cash.is_positive() {
                    self.entry_remaining = ctx.affordable_qty(
                        INSTRUMENT,
                        sizing_price,
                        sizing_cash,
                        SIZING_MARGIN_PPM,
                    );
                }
            }
        }

        self.history.push_back(hour);
        while self.history.len() > 48 {
            self.history.pop_front();
        }
    }

    fn submit_pending(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if self.exit_mode && ctx.position(INSTRUMENT).is_zero() {
            self.exit_mode = false;
        }

        if ctx.open_orders() != 0 {
            return;
        }

        // The just-closed minute is the latest volume observable when placing
        // an order for the next minute's open.
        let minute_cap = bar.volume.mul_ratio(1, 10);
        if !minute_cap.is_positive() {
            return;
        }

        if self.exit_mode {
            let position = ctx.position(INSTRUMENT);
            let qty = min_qty(position, minute_cap);
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
            }
            return;
        }

        if self.entry_remaining.is_positive() {
            let sizing_price = bar.close.mul_ratio(SLIPPAGE_NUM, SLIPPAGE_DEN);
            let sizing_cash: Money = ctx.cash().mul_ratio(95, 100);
            if !sizing_cash.is_positive() {
                return;
            }
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                sizing_price,
                sizing_cash,
                SIZING_MARGIN_PPM,
            );
            let qty = min_qty(min_qty(self.entry_remaining, affordable), minute_cap);
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        }
    }
}

fn min_qty(a: Qty, b: Qty) -> Qty {
    Qty::from_raw(a.raw().min(b.raw()).max(0))
}

impl Strategy for HourlyChannelBreakout {
    fn name(&self) -> String {
        "hourly-channel-breakout".to_owned()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let hour_ns = TimeDelta::from_hours(1).as_nanos();
        let close_ns = bar.close_ts.as_nanos();
        let hour_id = close_ns.saturating_sub(1).div_euclid(hour_ns);

        if self
            .building
            .as_ref()
            .is_some_and(|building| building.hour_id != hour_id)
        {
            if let Some(completed) = self.building.take() {
                self.finish_hour(ctx, completed.complete());
            }
        }

        match self.building.as_mut() {
            Some(building) => building.add(bar),
            None => self.building = Some(BuildingHour::new(hour_id, bar)),
        }

        if close_ns.rem_euclid(hour_ns) == 0 {
            if let Some(completed) = self.building.take() {
                self.finish_hour(ctx, completed.complete());
            }
        }

        self.submit_pending(ctx, bar);
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Buy {
            self.entry_remaining = Qty::from_raw(
                self.entry_remaining
                    .raw()
                    .saturating_sub(fill.qty.raw())
                    .max(0),
            );
        }
    }
}

export_strategy!(HourlyChannelBreakout::new());
