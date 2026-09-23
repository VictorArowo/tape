use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, TimeInForce,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const FLOW_WINDOW: usize = 6;
const RETURN_WINDOW: usize = 7;
const BUY_FLOW_THRESHOLD: f64 = 0.20;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct BuildingBar {
    bucket: i64,
    open: Price,
    high: Price,
    low: Price,
    close: Price,
    volume: tape_guest::Qty,
}

impl BuildingBar {
    fn new(bucket: i64, bar: &Bar) -> Self {
        Self {
            bucket,
            open: bar.open,
            high: bar.high,
            low: bar.low,
            close: bar.close,
            volume: bar.volume,
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
        self.volume = self.volume + bar.volume;
    }
}

struct SignedFlowPersistence {
    active: Option<BuildingBar>,
    last_completed_bucket: Option<i64>,
    signed_flows: VecDeque<(f64, f64)>,
    closes: VecDeque<Price>,
}

impl SignedFlowPersistence {
    fn new() -> Self {
        Self {
            active: None,
            last_completed_bucket: None,
            signed_flows: VecDeque::with_capacity(FLOW_WINDOW),
            closes: VecDeque::with_capacity(RETURN_WINDOW),
        }
    }

    fn on_completed_bar(&mut self, ctx: &mut Ctx, bar: BuildingBar) {
        if self
            .last_completed_bucket
            .and_then(|previous| previous.checked_add(1))
            != Some(bar.bucket)
        {
            self.signed_flows.clear();
            self.closes.clear();
        }
        self.last_completed_bucket = Some(bar.bucket);

        let volume = bar.volume.to_f64();
        let signed_volume = if bar.close > bar.open {
            volume
        } else if bar.close < bar.open {
            -volume
        } else {
            0.0
        };
        self.signed_flows.push_back((signed_volume, volume));
        if self.signed_flows.len() > FLOW_WINDOW {
            self.signed_flows.pop_front();
        }

        self.closes.push_back(bar.close);
        if self.closes.len() > RETURN_WINDOW {
            self.closes.pop_front();
        }

        if self.signed_flows.len() != FLOW_WINDOW || self.closes.len() != RETURN_WINDOW {
            return;
        }
        if ctx.open_orders() != 0 {
            return;
        }

        let signed_sum: f64 = self.signed_flows.iter().map(|(signed, _)| *signed).sum();
        let volume_sum: f64 = self.signed_flows.iter().map(|(_, volume)| *volume).sum();
        let flow = if volume_sum > 0.0 {
            signed_sum / volume_sum
        } else {
            0.0
        };

        let Some(old_close) = self.closes.front() else {
            return;
        };
        if old_close.to_f64() <= 0.0 {
            return;
        }
        let return_24h = bar.close.to_f64() / old_close.to_f64() - 1.0;

        let position = ctx.position(INSTRUMENT);
        if position.is_zero() {
            if flow >= BUY_FLOW_THRESHOLD && return_24h > 0.0 {
                let qty = ctx.affordable_qty(
                    INSTRUMENT,
                    bar.close,
                    ctx.cash(),
                    SIZING_MARGIN_PPM,
                );
                if qty.is_positive() {
                    ctx.submit(
                        OrderRequest::market(INSTRUMENT, Side::Buy, qty)
                            .with_time_in_force(TimeInForce::Ioc),
                    );
                }
            }
        } else if position.is_positive() && (flow <= 0.0 || return_24h <= 0.0) {
            ctx.submit(
                OrderRequest::market(INSTRUMENT, Side::Sell, position)
                    .with_time_in_force(TimeInForce::Ioc),
            );
        }
    }
}

impl Strategy for SignedFlowPersistence {
    fn name(&self) -> String {
        "Signed-Flow Persistence".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let period_ns = TimeDelta::from_hours(4).as_nanos();
        if period_ns <= 0 {
            return;
        }
        let bucket = bar.open_ts.as_nanos().div_euclid(period_ns);

        if self.active.as_ref().map(|active| active.bucket) != Some(bucket) {
            self.active = Some(BuildingBar::new(bucket, bar));
        } else if let Some(active) = self.active.as_mut() {
            active.add(bar);
        }

        let bucket_end_ns = (bucket + 1) * period_ns;
        if bar.close_ts.as_nanos() >= bucket_end_ns {
            if let Some(completed) = self.active.take() {
                self.on_completed_bar(ctx, completed);
            }
        }
    }
}

export_strategy!(SignedFlowPersistence::new());
