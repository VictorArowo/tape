use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderId, OrderRequest, Qty, Side, Strategy, TimeDelta,
    TimeInForce, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const ER_INTERVALS: usize = 24;
const EMA_PERIOD: usize = 72;
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy)]
enum IntentSide {
    Buy,
    Sell,
}

struct HourlyIndicators {
    active_hour: Option<i64>,
    active_close: Option<tape_guest::Price>,
    active_hour_finalized: bool,
    closes: VecDeque<i64>,
    ema_seed_count: usize,
    ema_seed_sum: f64,
    ema: Option<f64>,
    entry_signal: bool,
    exit_signal: bool,
}

impl HourlyIndicators {
    fn new() -> Self {
        Self {
            active_hour: None,
            active_close: None,
            active_hour_finalized: false,
            closes: VecDeque::with_capacity(ER_INTERVALS + 1),
            ema_seed_count: 0,
            ema_seed_sum: 0.0,
            ema: None,
            entry_signal: false,
            exit_signal: false,
        }
    }

    fn finish_hour(&mut self, close: tape_guest::Price) {
        let raw = close.raw();
        self.closes.push_back(raw);
        if self.closes.len() > ER_INTERVALS + 1 {
            self.closes.pop_front();
        }

        if self.ema.is_some() {
            let alpha = 2.0 / (EMA_PERIOD as f64 + 1.0);
            let previous = self.ema.expect("EMA was checked above");
            self.ema = Some(previous + alpha * (raw as f64 - previous));
        } else {
            self.ema_seed_count += 1;
            self.ema_seed_sum += raw as f64;
            if self.ema_seed_count == EMA_PERIOD {
                self.ema = Some(self.ema_seed_sum / EMA_PERIOD as f64);
            }
        }

        let Some(ema) = self.ema else {
            self.entry_signal = false;
            self.exit_signal = false;
            return;
        };
        if self.closes.len() != ER_INTERVALS + 1 {
            self.entry_signal = false;
            self.exit_signal = false;
            return;
        }

        let mut path_length = 0.0;
        for pair in self.closes.as_slices().0.windows(2) {
            path_length += ((pair[1] as f64) - (pair[0] as f64)).abs();
        }
        // VecDeque may wrap, so include adjacent pairs from its second slice as well.
        let (first, second) = self.closes.as_slices();
        if first.len() + second.len() == ER_INTERVALS + 1 {
            if first.len() > 1 {
                path_length = first
                    .windows(2)
                    .map(|p| ((p[1] as f64) - (p[0] as f64)).abs())
                    .sum();
            } else {
                path_length = 0.0;
            }
            path_length += second
                .windows(2)
                .map(|p| ((p[1] as f64) - (p[0] as f64)).abs())
                .sum::<f64>();
            if !first.is_empty() && !second.is_empty() {
                path_length += ((second[0] as f64) - (*first.last().expect("non-empty") as f64)).abs();
            }
        }

        let oldest = *self.closes.front().expect("24-hour window is ready") as f64;
        let newest = *self.closes.back().expect("24-hour window is ready") as f64;
        let efficiency = if path_length > 0.0 {
            (newest - oldest) / path_length
        } else {
            0.0
        };
        let close_value = raw as f64;
        self.entry_signal = efficiency >= 0.35 && close_value > ema;
        self.exit_signal = efficiency <= 0.15 || close_value <= ema;
    }

    fn push_minute(&mut self, bar: &Bar) -> bool {
        let hour_ns = TimeDelta::from_hours(1).as_nanos();
        let open_hour = bar.open_ts.as_nanos().div_euclid(hour_ns);
        let mut completed = false;

        if self.active_hour != Some(open_hour) {
            if self.active_hour.is_some() && !self.active_hour_finalized {
                if let Some(close) = self.active_close {
                    self.finish_hour(close);
                    completed = true;
                }
            }
            self.active_hour = Some(open_hour);
            self.active_close = None;
            self.active_hour_finalized = false;
        }

        self.active_close = Some(bar.close);
        let close_hour = bar.close_ts.as_nanos().div_euclid(hour_ns);
        if close_hour > open_hour && !self.active_hour_finalized {
            self.finish_hour(bar.close);
            self.active_hour_finalized = true;
            completed = true;
        }
        completed
    }
}

struct DirectionalEfficiencyGate {
    indicators: HourlyIndicators,
    intent: Option<IntentSide>,
    remaining: Qty,
    live_order: Option<OrderId>,
}

impl DirectionalEfficiencyGate {
    fn new() -> Self {
        Self {
            indicators: HourlyIndicators::new(),
            intent: None,
            remaining: Qty::from_raw(0),
            live_order: None,
        }
    }

    fn refresh_intent(&mut self, ctx: &mut Ctx, price: tape_guest::Price) {
        if ctx.open_orders() == 0 {
            self.live_order = None;
        }

        let position = ctx.position(INSTRUMENT);
        let intent_valid = match self.intent {
            Some(IntentSide::Buy) => self.indicators.entry_signal,
            Some(IntentSide::Sell) => self.indicators.exit_signal && position.is_positive(),
            None => true,
        };

        if self.intent.is_some() && !intent_valid {
            if let Some(order_id) = self.live_order {
                ctx.cancel(order_id);
            }
            self.intent = None;
            self.remaining = Qty::from_raw(0);
            if ctx.open_orders() == 0 {
                self.live_order = None;
            }
        }

        if self.intent.is_some() || self.live_order.is_some() || ctx.open_orders() != 0 {
            return;
        }

        if position.is_zero() && self.indicators.entry_signal {
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                price,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                self.intent = Some(IntentSide::Buy);
                self.remaining = qty;
            }
        } else if position.is_positive() && self.indicators.exit_signal {
            self.intent = Some(IntentSide::Sell);
            self.remaining = position;
        }
    }

    fn submit_if_possible(&mut self, ctx: &mut Ctx, volume: Qty) {
        if self.intent.is_none() || self.remaining.is_zero() {
            return;
        }
        if ctx.open_orders() != 0 || self.live_order.is_some() {
            return;
        }

        let bar_cap = volume.mul_ratio(1, 10);
        if !bar_cap.is_positive() {
            return;
        }
        let mut qty = if self.remaining.raw() < bar_cap.raw() {
            self.remaining
        } else {
            bar_cap
        };

        let (side, request_side) = match self.intent {
            Some(IntentSide::Buy) => (IntentSide::Buy, Side::Buy),
            Some(IntentSide::Sell) => {
                let position = ctx.position(INSTRUMENT);
                if !position.is_positive() {
                    self.intent = None;
                    self.remaining = Qty::from_raw(0);
                    return;
                }
                if qty.raw() > position.raw() {
                    qty = position;
                }
                (IntentSide::Sell, Side::Sell)
            }
            None => return,
        };
        if !qty.is_positive() {
            return;
        }

        let _ = side;
        let order_id = ctx.submit(
            OrderRequest::market(INSTRUMENT, request_side, qty)
                .with_time_in_force(TimeInForce::Ioc),
        );
        self.live_order = Some(order_id);
    }
}

impl Strategy for DirectionalEfficiencyGate {
    fn name(&self) -> String {
        "directional-efficiency-gate(ER24,EMA72)".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let completed_hour = self.indicators.push_minute(bar);
        if completed_hour {
            self.refresh_intent(ctx, bar.close);
        } else if ctx.open_orders() == 0 {
            self.live_order = None;
        }

        self.submit_if_possible(ctx, bar.volume);
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        let Some(order_id) = self.live_order else {
            return;
        };
        if fill.order_id != order_id {
            return;
        }

        let matching_side = matches!(
            (self.intent, fill.side),
            (Some(IntentSide::Buy), Side::Buy) | (Some(IntentSide::Sell), Side::Sell)
        );
        if !matching_side {
            return;
        }

        let remaining = self.remaining.raw().saturating_sub(fill.qty.raw()).max(0);
        self.remaining = Qty::from_raw(remaining);
        if self.remaining.is_zero() {
            self.intent = None;
        }
    }
}

export_strategy!(DirectionalEfficiencyGate::new());
