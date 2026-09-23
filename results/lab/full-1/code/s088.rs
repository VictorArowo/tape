use tape_guest::{
    Bar, Ctx, InstrumentId, OrderId, OrderRequest, Price, Side, Strategy, TimeDelta,
    TimeInForce, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy)]
enum PendingKind {
    Entry,
    Exit,
}

struct PendingOrder {
    id: OrderId,
    kind: PendingKind,
}

struct DrawdownRatchet {
    hour_bucket: Option<i64>,
    hourly_close: Option<Price>,
    invested: bool,
    reentry_enabled: bool,
    high_water: Option<Price>,
    low_water: Option<Price>,
    exit_requested: bool,
    pending: Option<PendingOrder>,
}

impl DrawdownRatchet {
    fn sync_position(&mut self, position_is_positive: bool) {
        if position_is_positive {
            if !self.invested {
                self.invested = true;
                self.reentry_enabled = false;
                self.high_water = None;
                self.low_water = None;
            }
        } else if self.invested {
            self.invested = false;
            self.reentry_enabled = true;
            self.high_water = None;
            self.low_water = None;
            self.exit_requested = false;
        }
    }

    fn cancel_pending_entry(&mut self, ctx: &mut Ctx) {
        let is_entry = matches!(
            self.pending.as_ref(),
            Some(pending) if matches!(pending.kind, PendingKind::Entry)
        );
        if is_entry {
            if let Some(pending) = self.pending.take() {
                ctx.cancel(pending.id);
            }
        }
    }

    fn submit_exit_if_ready(&mut self, ctx: &mut Ctx) {
        if !self.exit_requested || ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if !position.is_positive() {
            self.exit_requested = false;
            return;
        }

        let id = ctx.submit(
            OrderRequest::market(INSTRUMENT, Side::Sell, position)
                .with_time_in_force(TimeInForce::Gtc),
        );
        self.pending = Some(PendingOrder {
            id,
            kind: PendingKind::Exit,
        });
    }

    fn submit_entry(&mut self, ctx: &mut Ctx, price: Price) {
        if ctx.open_orders() != 0 {
            return;
        }

        let qty = ctx.affordable_qty(
            INSTRUMENT,
            price,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        if qty.is_positive() {
            let id = ctx.submit(
                OrderRequest::market(INSTRUMENT, Side::Buy, qty)
                    .with_time_in_force(TimeInForce::Gtc),
            );
            self.pending = Some(PendingOrder {
                id,
                kind: PendingKind::Entry,
            });
        }
    }

    fn on_hour_close(&mut self, ctx: &mut Ctx, close: Price) {
        let position = ctx.position(INSTRUMENT);
        self.sync_position(position.is_positive());

        if position.is_positive() {
            let high = self.high_water.get_or_insert(close);
            if close > *high {
                *high = close;
            }

            // A 5.5% decline means the close is at or below 94.5% of the high.
            if i128::from(close.raw()) * 1_000
                <= i128::from(high.raw()) * 945
            {
                self.exit_requested = true;
                self.cancel_pending_entry(ctx);
            }
        } else if self.reentry_enabled {
            let low = self.low_water.get_or_insert(close);
            if close < *low {
                *low = close;
            }

            // Re-enter when the close is at least 3.5% above the post-exit low.
            if i128::from(close.raw()) * 1_000
                >= i128::from(low.raw()) * 1_035
            {
                self.submit_entry(ctx, close);
            }
        }
    }
}

impl Strategy for DrawdownRatchet {
    fn name(&self) -> String {
        "drawdown-ratchet-re-entry".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let hour_ns = TimeDelta::from_hours(1).as_nanos();
        let open_minute = bar.open_ts.as_nanos().div_euclid(minute_ns);
        let bucket = open_minute.div_euclid(60);

        if self.hour_bucket != Some(bucket) {
            self.hour_bucket = Some(bucket);
        }
        self.hourly_close = Some(bar.close);

        self.sync_position(ctx.position(INSTRUMENT).is_positive());
        if ctx.open_orders() == 0 {
            self.pending = None;
        }

        // With minute bars, the :59 open is the final minute of the UTC hour;
        // the close timestamp boundary also handles bars labelled at hour-end.
        let hour_complete = open_minute.rem_euclid(60) == 59
            || bar.close_ts.as_nanos().rem_euclid(hour_ns) == 0;
        if hour_complete {
            if let Some(close) = self.hourly_close {
                self.on_hour_close(ctx, close);
            }
        }

        self.submit_exit_if_ready(ctx);
    }
}

export_strategy!(DrawdownRatchet {
    hour_bucket: None,
    hourly_close: None,
    invested: false,
    reentry_enabled: false,
    high_water: None,
    low_water: None,
    exit_requested: false,
    pending: None,
});
