use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const LOOKBACK: usize = 2_880;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct FifteenMinuteBar {
    open: i64,
    high: i64,
    low: i64,
    close: i64,
    volume: i128,
    minutes: usize,
}

impl FifteenMinuteBar {
    fn new(bar: &Bar) -> Self {
        Self {
            open: bar.open.raw(),
            high: bar.high.raw(),
            low: bar.low.raw(),
            close: bar.close.raw(),
            volume: i128::from(bar.volume.raw()),
            minutes: 1,
        }
    }

    fn add(&mut self, bar: &Bar) {
        self.high = self.high.max(bar.high.raw());
        self.low = self.low.min(bar.low.raw());
        self.close = bar.close.raw();
        self.volume += i128::from(bar.volume.raw());
        self.minutes += 1;
    }
}

struct LiquidationWickRebound {
    aggregate: Option<FifteenMinuteBar>,
    history: VecDeque<i64>,
    entry_price: Option<i64>,
    entry_ts: Option<i64>,
}

impl LiquidationWickRebound {
    fn finish_fifteen(&mut self, ctx: &mut Ctx, bar: FifteenMinuteBar) {
        let signal = if self.history.len() == LOOKBACK && ctx.position(INSTRUMENT).is_zero() && ctx.open_orders() == 0 {
            let mut prior: Vec<i64> = self.history.iter().copied().collect();
            prior.sort_unstable();
            let median = (i128::from(prior[LOOKBACK / 2 - 1]) + i128::from(prior[LOOKBACK / 2])) / 2;
            let selloff = i128::from(bar.close) * 1_000 <= i128::from(bar.open) * 975;
            let high_volume = bar.volume >= median * 3;
            let range = i128::from(bar.high) - i128::from(bar.low);
            let strong_close = range > 0
                && (i128::from(bar.close) - i128::from(bar.low)) * 4 >= range * 3;
            selloff && high_volume && strong_close
        } else {
            false
        };

        if signal {
            let budget = ctx.equity().mul_ratio(1, 4);
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                tape_guest::Price::from_raw(bar.close),
                budget,
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        }

        self.history.push_back(bar.volume.clamp(i64::MIN as i128, i64::MAX as i128) as i64);
        if self.history.len() > LOOKBACK {
            self.history.pop_front();
        }
    }
}

impl Strategy for LiquidationWickRebound {
    fn name(&self) -> String {
        "Liquidation-Wick Rebound".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        if let (Some(entry), Some(entry_ts)) = (self.entry_price, self.entry_ts) {
            if ctx.position(INSTRUMENT).is_positive() && ctx.open_orders() == 0 {
                let close = i128::from(bar.close.raw());
                let entry_wide = i128::from(entry);
                let stop = close * 100 <= entry_wide * 98;
                let target = close * 100 >= entry_wide * 103;
                let timed = bar.close_ts.as_nanos() - entry_ts
                    >= TimeDelta::from_hours(8).as_nanos();
                if stop || target || timed {
                    ctx.submit(OrderRequest::market(
                        INSTRUMENT,
                        Side::Sell,
                        ctx.position(INSTRUMENT),
                    ));
                }
            }
        }

        let minute = bar.close_ts.as_nanos() / TimeDelta::from_mins(1).as_nanos();
        let bucket = minute.div_euclid(15);
        match self.aggregate.as_mut() {
            Some(current) if current.minutes < 15 => current.add(bar),
            _ => self.aggregate = Some(FifteenMinuteBar::new(bar)),
        }

        if minute.rem_euclid(15) == 14 {
            if let Some(completed) = self.aggregate.take() {
                if completed.minutes == 15 {
                    self.finish_fifteen(ctx, completed);
                } else {
                    self.aggregate = Some(completed);
                }
            }
        } else if let Some(current) = self.aggregate.as_ref() {
            let _ = bucket;
            let _ = current.minutes;
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.side == Side::Buy {
            self.entry_price = Some(fill.price.raw());
            self.entry_ts = Some(fill.ts.as_nanos());
        } else {
            self.entry_price = None;
            self.entry_ts = None;
        }
    }
}

export_strategy!(LiquidationWickRebound {
    aggregate: None,
    history: VecDeque::with_capacity(LOOKBACK + 1),
    entry_price: None,
    entry_ts: None,
});
