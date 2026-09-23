use tape_guest::{
    export_strategy, Bar, Ctx, Fill, InstrumentId, OrderRequest, Price, Qty, Side, Strategy,
    TimeDelta,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

struct TrailingExitRecovery {
    bucket: Option<i64>,
    bucket_close: Option<Price>,
    peak_close: Option<Price>,
    exit_fill_price: Option<Price>,
    pending_entry: bool,
    pending_exit: bool,
}

impl TrailingExitRecovery {
    fn update_four_hour_bar(&mut self, bar: &Bar) -> Option<Price> {
        let interval = TimeDelta::from_hours(4).as_nanos();
        if interval <= 0 {
            return None;
        }

        let open_bucket = bar.open_ts.as_nanos().div_euclid(interval);
        let close_bucket = bar.close_ts.as_nanos().div_euclid(interval);
        let mut completed = None;

        if self.bucket != Some(open_bucket) {
            if self.bucket.is_some() {
                completed = self.bucket_close.take();
            }
            self.bucket = Some(open_bucket);
        }

        self.bucket_close = Some(bar.close);

        if close_bucket > open_bucket {
            completed = self.bucket_close.take();
            self.bucket = None;
        }

        completed
    }

    fn on_four_hour_close(&mut self, ctx: &Ctx, close: Price) {
        if ctx.position(INSTRUMENT).is_positive() {
            self.pending_entry = false;
            let peak = match self.peak_close {
                Some(previous) if previous > close => previous,
                _ => close,
            };
            self.peak_close = Some(peak);

            if close <= peak.mul_ratio(95, 100) {
                self.pending_exit = true;
            }
        } else if !self.pending_exit {
            if let Some(exit_price) = self.exit_fill_price {
                if close >= exit_price.mul_ratio(103, 100) {
                    self.pending_entry = true;
                }
            }
        }
    }
}

impl Strategy for TrailingExitRecovery {
    fn name(&self) -> String {
        "trailing-exit-recovery".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        if let Some(close) = self.update_four_hour_bar(bar) {
            self.on_four_hour_close(ctx, close);
        }

        if ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if self.pending_exit && position.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            return;
        }

        if self.pending_entry && position.is_zero() {
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            let bar_volume_cap = bar.volume.mul_ratio(1, 10);
            if qty.is_positive() && qty <= bar_volume_cap {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Buy {
            self.pending_entry = false;
            self.pending_exit = false;
            self.peak_close = None;
        } else if fill.side == Side::Sell {
            if ctx.position(INSTRUMENT).is_zero() {
                self.exit_fill_price = Some(fill.price);
                self.peak_close = None;
                self.pending_exit = false;
            } else {
                self.pending_exit = true;
            }
        }
    }

    fn on_finish(&mut self, ctx: &mut Ctx) {
        let position: Qty = ctx.position(INSTRUMENT);
        if ctx.open_orders() == 0 && position.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
        }
    }
}

export_strategy!(TrailingExitRecovery {
    bucket: None,
    bucket_close: None,
    peak_close: None,
    exit_fill_price: None,
    pending_entry: true,
    pending_exit: false,
});
