use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, Fill, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const DRAWDOWN_NUM: i64 = 20;
const DRAWDOWN_DEN: i64 = 100;
const SIZING_MARGIN_PPM: i64 = 0;
const LOOKBACK_30D: u64 = 43_200;
const LOOKBACK_3D: u64 = 4_320;

struct RollingHigh {
    values: VecDeque<(u64, i64)>,
    maxima: VecDeque<(u64, i64)>,
    window: u64,
}

impl RollingHigh {
    fn new(window: u64) -> Self {
        Self {
            values: VecDeque::new(),
            maxima: VecDeque::new(),
            window,
        }
    }

    fn previous_high(&mut self, index: u64) -> Option<i64> {
        let oldest = index.saturating_sub(self.window);
        while self.values.front().is_some_and(|(i, _)| *i < oldest) {
            self.values.pop_front();
        }
        while self.maxima.front().is_some_and(|(i, _)| *i < oldest) {
            self.maxima.pop_front();
        }
        if index < self.window {
            None
        } else {
            self.maxima.front().map(|(_, price)| *price)
        }
    }

    fn push(&mut self, index: u64, price: Price) {
        let raw = price.raw();
        self.values.push_back((index, raw));
        while self.maxima.back().is_some_and(|(_, value)| *value <= raw) {
            self.maxima.pop_back();
        }
        self.maxima.push_back((index, raw));
    }
}

struct DeepDrawdownRecovery {
    high_30d: RollingHigh,
    high_3d: RollingHigh,
    index: u64,
    entry_high: Option<Price>,
    entry_ts_nanos: Option<i64>,
    pending_entry_high: Option<Price>,
}

impl DeepDrawdownRecovery {
    fn new() -> Self {
        Self {
            high_30d: RollingHigh::new(LOOKBACK_30D),
            high_3d: RollingHigh::new(LOOKBACK_3D),
            index: 0,
            entry_high: None,
            entry_ts_nanos: None,
            pending_entry_high: None,
        }
    }
}

impl Strategy for DeepDrawdownRecovery {
    fn name(&self) -> String {
        "deep-drawdown-recovery".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let high_30d = self.high_30d.previous_high(self.index);
        let high_3d = self.high_3d.previous_high(self.index);

        if ctx.open_orders() == 0 {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                let reached_entry_high = self
                    .entry_high
                    .is_some_and(|high| bar.close >= high);
                let time_expired = self.entry_ts_nanos.is_some_and(|entry| {
                    bar.close_ts.as_nanos() - entry >= TimeDelta::from_days(90).as_nanos()
                });
                if reached_entry_high || time_expired {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                }
            } else if position.is_zero() {
                if let (Some(high), Some(recovery_high)) = (high_30d, high_3d) {
                    let drawdown_limit = (i128::from(high)
                        * i128::from(DRAWDOWN_DEN - DRAWDOWN_NUM)
                        / i128::from(DRAWDOWN_DEN)) as i64;
                    if bar.close.raw() <= drawdown_limit && bar.close.raw() > recovery_high {
                        let qty = ctx.affordable_qty(
                            INSTRUMENT,
                            bar.close,
                            ctx.cash(),
                            SIZING_MARGIN_PPM,
                        );
                        if qty.is_positive() {
                            self.pending_entry_high = Some(Price::from_raw(high));
                            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                        }
                    }
                }
            }
        }

        self.high_30d.push(self.index, bar.close);
        self.high_3d.push(self.index, bar.close);
        self.index = self.index.saturating_add(1);
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Buy {
            self.entry_high = self.pending_entry_high.take();
            self.entry_ts_nanos = Some(fill.ts.as_nanos());
        } else {
            self.entry_high = None;
            self.entry_ts_nanos = None;
            self.pending_entry_high = None;
        }
    }
}

export_strategy!(DeepDrawdownRecovery::new());
