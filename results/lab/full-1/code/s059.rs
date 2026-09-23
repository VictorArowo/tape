use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, TimeInForce,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const DECLINE_BARS: usize = 12;
const REFERENCE_BARS: usize = 288;
const HOLD_MINUTES: i64 = 360;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct FiveMinuteBar {
    high: i64,
    low: i64,
    close: i64,
    volume: f64,
}

struct Aggregator {
    bucket: Option<i64>,
    count: usize,
    high: i64,
    low: i64,
    close: i64,
    volume: f64,
}

impl Aggregator {
    fn new() -> Self {
        Self {
            bucket: None,
            count: 0,
            high: 0,
            low: 0,
            close: 0,
            volume: 0.0,
        }
    }

    fn push(&mut self, bar: &Bar) -> Option<FiveMinuteBar> {
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let bucket = (bar.open_ts.as_nanos() / minute_ns) / 5;

        if self.bucket != Some(bucket) {
            self.bucket = Some(bucket);
            self.count = 0;
        }

        if self.count == 0 {
            self.high = bar.high.raw();
            self.low = bar.low.raw();
            self.volume = 0.0;
        } else {
            self.high = self.high.max(bar.high.raw());
            self.low = self.low.min(bar.low.raw());
        }

        self.close = bar.close.raw();
        self.volume += bar.volume.raw() as f64;
        self.count += 1;

        if self.count == 5 {
            self.count = 0;
            Some(FiveMinuteBar {
                high: self.high,
                low: self.low,
                close: self.close,
                volume: self.volume,
            })
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TradeState {
    Flat,
    EntryPending,
    Holding,
    ExitPending,
}

struct DownsideVolumeAbsorption {
    aggregator: Aggregator,
    closes: VecDeque<i64>,
    volumes: VecDeque<f64>,
    true_ranges: VecDeque<f64>,
    last_close: Option<i64>,
    state: TradeState,
    entry_ts_ns: Option<i64>,
}

impl DownsideVolumeAbsorption {
    fn new() -> Self {
        Self {
            aggregator: Aggregator::new(),
            closes: VecDeque::with_capacity(DECLINE_BARS + 1),
            volumes: VecDeque::with_capacity(REFERENCE_BARS + 1),
            true_ranges: VecDeque::with_capacity(REFERENCE_BARS + 1),
            last_close: None,
            state: TradeState::Flat,
            entry_ts_ns: None,
        }
    }

    fn median(values: &VecDeque<f64>) -> f64 {
        let mut sorted: Vec<f64> = values.iter().copied().collect();
        sorted.sort_by(f64::total_cmp);
        let middle = sorted.len() / 2;
        if sorted.len() % 2 == 0 {
            (sorted[middle - 1] + sorted[middle]) / 2.0
        } else {
            sorted[middle]
        }
    }

    // Evaluate against history before adding this completed five-minute bar.
    fn observe_five_minute_bar(&mut self, bar: FiveMinuteBar) -> bool {
        let high = bar.high as f64;
        let low = bar.low as f64;
        let close = bar.close as f64;
        let range = high - low;
        let true_range = if let Some(previous_close) = self.last_close {
            let previous_close = previous_close as f64;
            range
                .max((high - previous_close).abs())
                .max((low - previous_close).abs())
        } else {
            range
        };

        let signal = if self.closes.len() == DECLINE_BARS
            && self.volumes.len() == REFERENCE_BARS
            && self.true_ranges.len() == REFERENCE_BARS
            && bar.high != bar.low
        {
            let old_close = self.closes.front().copied().unwrap_or(bar.close);
            let declined = i128::from(bar.close) * 100 <= i128::from(old_close) * 99;
            let high_volume = bar.volume > 2.0 * Self::median(&self.volumes);
            let contained_true_range = true_range <= 0.8 * Self::median(&self.true_ranges);
            let near_high = i128::from(bar.close - bar.low) * 4
                >= i128::from(bar.high - bar.low) * 3;
            declined && high_volume && contained_true_range && near_high
        } else {
            false
        };

        self.closes.push_back(bar.close);
        if self.closes.len() > DECLINE_BARS {
            self.closes.pop_front();
        }
        self.volumes.push_back(bar.volume);
        if self.volumes.len() > REFERENCE_BARS {
            self.volumes.pop_front();
        }
        self.true_ranges.push_back(true_range);
        if self.true_ranges.len() > REFERENCE_BARS {
            self.true_ranges.pop_front();
        }
        self.last_close = Some(bar.close);

        signal
    }
}

impl Strategy for DownsideVolumeAbsorption {
    fn name(&self) -> String {
        "downside-volume-absorption".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let completed = self.aggregator.push(bar);
        let signal = if let Some(five_minute_bar) = completed {
            self.observe_five_minute_bar(five_minute_bar)
        } else {
            false
        };

        match self.state {
            TradeState::EntryPending => {
                if ctx.open_orders() != 0 {
                    return;
                }
                if ctx.position(INSTRUMENT).is_positive() {
                    self.state = TradeState::Holding;
                    if self.entry_ts_ns.is_none() {
                        self.entry_ts_ns = Some(bar.open_ts.as_nanos());
                    }
                } else {
                    self.state = TradeState::Flat;
                    self.entry_ts_ns = None;
                }
            }
            TradeState::Holding => {
                if ctx.open_orders() != 0 {
                    return;
                }
                let position = ctx.position(INSTRUMENT);
                if !position.is_positive() {
                    self.state = TradeState::Flat;
                    self.entry_ts_ns = None;
                } else if let Some(entry_ts_ns) = self.entry_ts_ns {
                    let elapsed = bar.close_ts.as_nanos().saturating_sub(entry_ts_ns);
                    if elapsed >= TimeDelta::from_mins(HOLD_MINUTES).as_nanos() {
                        if ctx.open_orders() == 0 {
                            ctx.submit(
                                OrderRequest::market(INSTRUMENT, Side::Sell, position)
                                    .with_time_in_force(TimeInForce::Ioc),
                            );
                            self.state = TradeState::ExitPending;
                        }
                        return;
                    } else {
                        return;
                    }
                } else {
                    self.entry_ts_ns = Some(bar.open_ts.as_nanos());
                    return;
                }
            }
            TradeState::ExitPending => {
                if ctx.open_orders() != 0 {
                    return;
                }
                let position = ctx.position(INSTRUMENT);
                if position.is_positive() {
                    if ctx.open_orders() == 0 {
                        ctx.submit(
                            OrderRequest::market(INSTRUMENT, Side::Sell, position)
                                .with_time_in_force(TimeInForce::Ioc),
                        );
                    }
                    return;
                }
                self.state = TradeState::Flat;
                self.entry_ts_ns = None;
            }
            TradeState::Flat => {}
        }

        if self.state != TradeState::Flat || !signal || ctx.open_orders() != 0 {
            return;
        }
        if ctx.position(INSTRUMENT).is_positive() {
            return;
        }

        let qty = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        if qty.is_positive() && ctx.open_orders() == 0 {
            ctx.submit(
                OrderRequest::market(INSTRUMENT, Side::Buy, qty)
                    .with_time_in_force(TimeInForce::Ioc),
            );
            self.state = TradeState::EntryPending;
            self.entry_ts_ns = None;
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.side == Side::Buy && self.state == TradeState::EntryPending {
            self.state = TradeState::Holding;
            if self.entry_ts_ns.is_none() {
                self.entry_ts_ns = Some(fill.ts.as_nanos());
            }
        }
    }
}

export_strategy!(DownsideVolumeAbsorption::new());
