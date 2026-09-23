use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const LOOKBACK: usize = 720;
const RETAINED_BARS: usize = LOOKBACK + 1;
const SIZING_MARGIN_PPM: i64 = 5_000;
const FLOW_THRESHOLD: f64 = 0.20;
const RETURN_THRESHOLD: f64 = 0.01;

struct FlowBar {
    signed_volume: f64,
    volume: f64,
    close_raw: i64,
}

struct PersistentSignedVolumeFlow {
    history: VecDeque<FlowBar>,
    pending_active: bool,
    pending_is_buy: bool,
    pending_remaining_raw: i64,
    last_exit_ns: Option<i64>,
}

impl PersistentSignedVolumeFlow {
    fn push_bar(&mut self, bar: &Bar) {
        let high = bar.high.to_f64();
        let low = bar.low.to_f64();
        let close = bar.close.to_f64();
        let volume = bar.volume.to_f64();
        let signed_volume = if bar.high > bar.low {
            volume * (2.0 * close - high - low) / (high - low)
        } else {
            0.0
        };

        self.history.push_back(FlowBar {
            signed_volume,
            volume,
            close_raw: bar.close.raw(),
        });
        while self.history.len() > RETAINED_BARS {
            self.history.pop_front();
        }
    }

    fn signals(&self) -> Option<(f64, f64)> {
        if self.history.len() < RETAINED_BARS {
            return None;
        }

        let mut signed_volume_sum = 0.0;
        let mut volume_sum = 0.0;
        for sample in self.history.iter().rev().take(LOOKBACK) {
            signed_volume_sum += sample.signed_volume;
            volume_sum += sample.volume;
        }

        let flow = if volume_sum > 0.0 {
            signed_volume_sum / volume_sum
        } else {
            0.0
        };
        let earlier_close = self.history.front()?.close_raw;
        let latest_close = self.history.back()?.close_raw;
        if earlier_close == 0 {
            return None;
        }
        let return_12h = latest_close as f64 / earlier_close as f64 - 1.0;
        Some((flow, return_12h))
    }

    fn submit_new_order(&mut self, ctx: &mut Ctx, is_buy: bool, qty: Qty) {
        if !qty.is_positive() || self.pending_active || ctx.open_orders() != 0 {
            return;
        }

        self.pending_active = true;
        self.pending_is_buy = is_buy;
        self.pending_remaining_raw = qty.raw();
        let side = if is_buy { Side::Buy } else { Side::Sell };
        let _ = ctx.submit(OrderRequest::market(INSTRUMENT, side, qty));
    }

    // Returns true when an existing order still occupies this bar's decision.
    fn continue_pending_order(&mut self, ctx: &mut Ctx, reference_price: Price) -> bool {
        if !self.pending_active {
            return false;
        }
        if ctx.open_orders() != 0 {
            return true;
        }
        if self.pending_remaining_raw <= 0 {
            self.pending_active = false;
            return false;
        }

        let remaining = Qty::from_raw(self.pending_remaining_raw);
        let qty = if self.pending_is_buy {
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                reference_price,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            Qty::from_raw(remaining.raw().min(affordable.raw()))
        } else {
            let position = ctx.position(INSTRUMENT);
            if !position.is_positive() {
                self.pending_remaining_raw = 0;
                self.pending_active = false;
                return false;
            }
            Qty::from_raw(remaining.raw().min(position.raw()))
        };

        if !qty.is_positive() {
            self.pending_remaining_raw = 0;
            self.pending_active = false;
            return false;
        }

        let side = if self.pending_is_buy {
            Side::Buy
        } else {
            Side::Sell
        };
        let _ = ctx.submit(OrderRequest::market(INSTRUMENT, side, qty));
        true
    }
}

impl Strategy for PersistentSignedVolumeFlow {
    fn name(&self) -> String {
        "persistent-signed-volume-flow".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        self.push_bar(bar);

        if self.continue_pending_order(ctx, bar.close) {
            return;
        }
        if ctx.open_orders() != 0 {
            return;
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        if (bar.close_ts.as_nanos() / minute_ns) % 60 != 0 {
            return;
        }

        let Some((flow, return_12h)) = self.signals() else {
            return;
        };
        let position = ctx.position(INSTRUMENT);

        if position.is_zero() {
            let cooldown_ns = TimeDelta::from_hours(6).as_nanos();
            let cooldown_over = self.last_exit_ns.is_none_or(|exit_ns| {
                bar.close_ts.as_nanos() >= exit_ns.saturating_add(cooldown_ns)
            });
            if cooldown_over && flow >= FLOW_THRESHOLD && return_12h >= RETURN_THRESHOLD {
                let qty = ctx.affordable_qty(
                    INSTRUMENT,
                    bar.close,
                    ctx.cash(),
                    SIZING_MARGIN_PPM,
                );
                self.submit_new_order(ctx, true, qty);
            }
        } else if position.is_positive() && (flow <= 0.0 || return_12h <= 0.0) {
            self.submit_new_order(ctx, false, position);
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if !self.pending_active {
            return;
        }

        self.pending_remaining_raw =
            (self.pending_remaining_raw - fill.qty.raw()).max(0);
        if !self.pending_is_buy {
            self.last_exit_ns = Some(fill.ts.as_nanos());
        }
    }
}

export_strategy!(PersistentSignedVolumeFlow {
    history: VecDeque::with_capacity(RETAINED_BARS),
    pending_active: false,
    pending_is_buy: false,
    pending_remaining_raw: 0,
    last_exit_ns: None,
});
