use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const FOUR_HOUR_MINUTES: usize = 240;
const ATR_PERIOD: usize = 14;
const ENTRY_ATR_MULTIPLE: f64 = 2.0;
const EXIT_ATR_MULTIPLE: f64 = 3.0;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct FourHourBar {
    _open: i64,
    high: i64,
    low: i64,
    close: i64,
}

struct FourHourBuilder {
    bucket: i64,
    open: i64,
    high: i64,
    low: i64,
    close: i64,
    count: usize,
}

impl FourHourBuilder {
    fn new(bucket: i64, bar: &Bar) -> Self {
        Self {
            bucket,
            open: bar.open.raw(),
            high: bar.high.raw(),
            low: bar.low.raw(),
            close: bar.close.raw(),
            count: 1,
        }
    }

    fn add(&mut self, bar: &Bar) {
        self.high = self.high.max(bar.high.raw());
        self.low = self.low.min(bar.low.raw());
        self.close = bar.close.raw();
        self.count += 1;
    }

    fn finish(self) -> FourHourBar {
        FourHourBar {
            _open: self.open,
            high: self.high,
            low: self.low,
            close: self.close,
        }
    }
}

struct VolatilityRatchet {
    current: Option<FourHourBuilder>,
    true_ranges: VecDeque<f64>,
    previous_close: Option<f64>,
    flat_low: Option<f64>,
    long_high_close: Option<f64>,
}

impl VolatilityRatchet {
    fn process_four_hour(&mut self, ctx: &mut Ctx, bar: FourHourBar, close_price: Price) {
        let high = bar.high as f64;
        let low = bar.low as f64;
        let close = bar.close as f64;

        let true_range = match self.previous_close {
            Some(previous) => (high - low)
                .max((high - previous).abs())
                .max((low - previous).abs()),
            None => high - low,
        };
        self.true_ranges.push_back(true_range);
        if self.true_ranges.len() > ATR_PERIOD {
            self.true_ranges.pop_front();
        }
        self.previous_close = Some(close);

        let atr = if self.true_ranges.len() == ATR_PERIOD {
            Some(self.true_ranges.iter().copied().sum::<f64>() / ATR_PERIOD as f64)
        } else {
            None
        };

        if ctx.position(INSTRUMENT).is_positive() {
            self.flat_low = None;
            let highest = self.long_high_close.map_or(close, |value| value.max(close));
            self.long_high_close = Some(highest);

            if let Some(atr) = atr {
                if highest - close >= EXIT_ATR_MULTIPLE * atr && ctx.open_orders() == 0 {
                    let position = ctx.position(INSTRUMENT);
                    if position.is_positive() {
                        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                    }
                }
            }
        } else {
            self.long_high_close = None;
            let lowest = self.flat_low.map_or(low, |value| value.min(low));
            self.flat_low = Some(lowest);

            if let Some(atr) = atr {
                if close - lowest >= ENTRY_ATR_MULTIPLE * atr && ctx.open_orders() == 0 {
                    let qty = ctx.affordable_qty(
                        INSTRUMENT,
                        close_price,
                        ctx.cash(),
                        SIZING_MARGIN_PPM,
                    );
                    if qty.is_positive() {
                        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                    }
                }
            }
        }
    }
}

impl Strategy for VolatilityRatchet {
    fn name(&self) -> String {
        String::from("volatility-ratchet(4h,14,2,3)")
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let four_hours_ns = TimeDelta::from_hours(4).as_nanos();
        let bucket = bar.open_ts.as_nanos().div_euclid(four_hours_ns);
        if self.current.as_ref().map(|builder| builder.bucket) != Some(bucket) {
            self.current = Some(FourHourBuilder::new(bucket, bar));
        } else if let Some(builder) = self.current.as_mut() {
            builder.add(bar);
        }

        let close_bucket = bar.close_ts.as_nanos().div_euclid(four_hours_ns);
        if close_bucket > bucket {
            let completed = self.current.take().and_then(|builder| {
                if builder.bucket == bucket && builder.count == FOUR_HOUR_MINUTES {
                    Some(builder.finish())
                } else {
                    None
                }
            });
            if let Some(four_hour_bar) = completed {
                self.process_four_hour(ctx, four_hour_bar, bar.close);
            }
        }
    }
}

export_strategy!(VolatilityRatchet {
    current: None,
    true_ranges: VecDeque::with_capacity(ATR_PERIOD + 1),
    previous_close: None,
    flat_low: None,
    long_high_close: None,
});