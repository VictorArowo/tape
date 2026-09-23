use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, Fill, InstrumentId, OrderId, OrderRequest, Side, Strategy, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const WINDOW: usize = 120;
const ENTRY_THRESHOLD: f64 = 0.20;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct SignedVolumePressure {
    window: VecDeque<(f64, f64)>, // (volume * CLV, volume)
    weighted_volume_sum: f64,
    volume_sum: f64,
    pending: Option<(OrderId, bool, bool)>, // (order id, target long, cancel requested)
    held_raw: i128,
    completed_round_trips: u64,
}

impl SignedVolumePressure {
    fn push_bar(&mut self, bar: &Bar) {
        let high = bar.high.to_f64();
        let low = bar.low.to_f64();
        let close = bar.close.to_f64();
        let clv = if high == low {
            0.0
        } else {
            (2.0 * close - high - low) / (high - low)
        };
        let volume = bar.volume.to_f64();
        let weighted_volume = volume * clv;

        self.window.push_back((weighted_volume, volume));
        self.weighted_volume_sum += weighted_volume;
        self.volume_sum += volume;

        if self.window.len() > WINDOW {
            if let Some((old_weighted, old_volume)) = self.window.pop_front() {
                self.weighted_volume_sum -= old_weighted;
                self.volume_sum -= old_volume;
            }
        }
    }

    fn pressure(&self) -> Option<f64> {
        if self.window.len() != WINDOW || self.volume_sum <= 0.0 {
            None
        } else {
            Some(self.weighted_volume_sum / self.volume_sum)
        }
    }
}

impl Strategy for SignedVolumePressure {
    fn name(&self) -> String {
        "signed-volume-pressure(120)".to_string()
    }

    fn on_start(&mut self, ctx: &mut Ctx) {
        self.held_raw = i128::from(ctx.position(INSTRUMENT).raw());
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        self.push_bar(bar);
        let Some(pressure) = self.pressure() else {
            return;
        };

        let position = ctx.position(INSTRUMENT);
        let target_long = if position.is_positive() {
            pressure > 0.0
        } else {
            pressure > ENTRY_THRESHOLD
        };

        if let Some((order_id, order_target, cancel_requested)) = self
            .pending
            .as_ref()
            .map(|(id, target, cancelled)| (id.clone(), *target, *cancelled))
        {
            if order_target != target_long && !cancel_requested {
                ctx.cancel(order_id);
                if let Some((_, _, cancelled)) = self.pending.as_mut() {
                    *cancelled = true;
                }
            }

            if ctx.open_orders() > 0 {
                return;
            }
            self.pending = None;
        }

        // Do not add another order if any order is still outstanding.
        if ctx.open_orders() != 0 {
            return;
        }

        if target_long && !position.is_positive() {
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                let order_id = ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                self.pending = Some((order_id, true, false));
            }
        } else if !target_long && position.is_positive() {
            let order_id = ctx.submit(OrderRequest::market(
                INSTRUMENT,
                Side::Sell,
                position,
            ));
            self.pending = Some((order_id, false, false));
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &Fill) {
        let previous = self.held_raw;
        let qty = i128::from(fill.qty.raw());
        match &fill.side {
            Side::Buy => self.held_raw += qty,
            Side::Sell => self.held_raw = (self.held_raw - qty).max(0),
        }
        if previous > 0 && self.held_raw == 0 {
            self.completed_round_trips += 1;
        }
    }

    fn on_finish(&mut self, ctx: &mut Ctx) {
        if self.completed_round_trips < 30 {
            ctx.log(&format!(
                "REJECT: only {} completed round trips; fewer than 30",
                self.completed_round_trips
            ));
        } else {
            ctx.log(&format!(
                "{} completed round trips (minimum 30 met)",
                self.completed_round_trips
            ));
        }
    }
}

export_strategy!(SignedVolumePressure {
    window: VecDeque::with_capacity(WINDOW + 1),
    weighted_volume_sum: 0.0,
    volume_sum: 0.0,
    pending: None,
    held_raw: 0,
    completed_round_trips: 0,
});