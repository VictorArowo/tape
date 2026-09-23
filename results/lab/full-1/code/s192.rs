use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderId, OrderRequest, Price, Qty, Side, Strategy,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const ENTRY_LOOKBACK: u64 = 14_400;
const EXIT_LOOKBACK: u64 = 7_200;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct PriceChannel {
    highs: VecDeque<(u64, i64)>,
    lows: VecDeque<(u64, i64)>,
    bars_seen: u64,
}

impl PriceChannel {
    fn new() -> Self {
        Self {
            highs: VecDeque::with_capacity(ENTRY_LOOKBACK as usize),
            lows: VecDeque::with_capacity(EXIT_LOOKBACK as usize),
            bars_seen: 0,
        }
    }

    fn prune_before(queue: &mut VecDeque<(u64, i64)>, cutoff: u64) {
        while queue.front().is_some_and(|(index, _)| *index < cutoff) {
            queue.pop_front();
        }
    }

    // Called before adding the current bar, so both channel values exclude it.
    fn preceding_channels(&mut self) -> (Option<i64>, Option<i64>) {
        let high_cutoff = self.bars_seen.saturating_sub(ENTRY_LOOKBACK);
        let low_cutoff = self.bars_seen.saturating_sub(EXIT_LOOKBACK);
        Self::prune_before(&mut self.highs, high_cutoff);
        Self::prune_before(&mut self.lows, low_cutoff);

        let high = (self.bars_seen >= ENTRY_LOOKBACK)
            .then(|| self.highs.front().map(|(_, value)| *value))
            .flatten();
        let low = (self.bars_seen >= EXIT_LOOKBACK)
            .then(|| self.lows.front().map(|(_, value)| *value))
            .flatten();
        (high, low)
    }

    fn push(&mut self, bar: &Bar) {
        let index = self.bars_seen;
        let high = bar.high.raw();
        let low = bar.low.raw();

        while self.highs.back().is_some_and(|(_, value)| *value <= high) {
            self.highs.pop_back();
        }
        self.highs.push_back((index, high));

        while self.lows.back().is_some_and(|(_, value)| *value >= low) {
            self.lows.pop_back();
        }
        self.lows.push_back((index, low));

        self.bars_seen = self.bars_seen.saturating_add(1);
    }
}

struct TenDayChannelTrend {
    channel: PriceChannel,
    entry_target_raw: Option<i64>,
    entry_filled_raw: i64,
    active_order: Option<OrderId>,
    exiting: bool,
}

impl TenDayChannelTrend {
    fn cancel_active(&mut self, ctx: &mut Ctx) {
        if let Some(order_id) = self.active_order.as_ref().cloned() {
            ctx.cancel(order_id);
        }
        self.active_order = None;
    }

    fn capped_order_qty(wanted: Qty, bar_volume: Qty) -> Qty {
        let cap = bar_volume.mul_ratio(1, 10);
        Qty::from_raw(wanted.raw().min(cap.raw()))
    }

    fn submit_entry(&mut self, ctx: &mut Ctx, bar: &Bar, wanted: Qty) {
        if ctx.open_orders() != 0 {
            return;
        }
        let qty = Self::capped_order_qty(wanted, bar.volume);
        if qty.is_positive() {
            self.active_order = Some(ctx.submit(OrderRequest::market(
                INSTRUMENT,
                Side::Buy,
                qty,
            )));
        }
    }

    fn submit_exit(&mut self, ctx: &mut Ctx, bar: &Bar, position: Qty) {
        if ctx.open_orders() != 0 {
            return;
        }
        let qty = Self::capped_order_qty(position, bar.volume);
        if qty.is_positive() {
            self.active_order = Some(ctx.submit(OrderRequest::market(
                INSTRUMENT,
                Side::Sell,
                qty,
            )));
        }
    }

    fn handle_signal(&mut self, ctx: &mut Ctx, bar: &Bar, prior_high: Option<i64>, prior_low: Option<i64>) {
        let enter_signal = prior_high.is_some_and(|high| bar.close.raw() > high);
        let exit_signal = prior_low.is_some_and(|low| bar.close.raw() < low);
        let position = ctx.position(INSTRUMENT);

        if ctx.open_orders() == 0 {
            self.active_order = None;
        }

        if exit_signal && position.is_positive() {
            self.exiting = true;
        }

        if self.exiting {
            self.entry_target_raw = None;
            self.entry_filled_raw = 0;
            if position.is_positive() {
                if ctx.open_orders() != 0 {
                    self.cancel_active(ctx);
                    return;
                }
                self.submit_exit(ctx, bar, position);
                return;
            }
            self.exiting = false;
        }

        if !enter_signal && self.entry_target_raw.is_some() {
            if ctx.open_orders() != 0 {
                self.cancel_active(ctx);
            }
            self.entry_target_raw = None;
            self.entry_filled_raw = 0;
            return;
        }

        if ctx.open_orders() != 0 {
            return;
        }

        if self.entry_target_raw.is_none() && enter_signal && position.is_zero() {
            // Allow for the stated buy-side slippage when sizing. The SDK's
            // affordability helper accounts for fees.
            let sizing_price = bar.close.mul_ratio(10_002, 10_000);
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                sizing_price,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if affordable.is_positive() {
                self.entry_target_raw = Some(affordable.raw());
                self.entry_filled_raw = 0;
            }
        }

        if let Some(target_raw) = self.entry_target_raw {
            let remaining_raw = target_raw.saturating_sub(self.entry_filled_raw);
            if remaining_raw <= 0 {
                self.entry_target_raw = None;
                self.entry_filled_raw = 0;
                return;
            }
            self.submit_entry(ctx, bar, Qty::from_raw(remaining_raw));
        }
    }
}

impl Strategy for TenDayChannelTrend {
    fn name(&self) -> String {
        "ten-day-channel-trend".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let (prior_high, prior_low) = self.channel.preceding_channels();
        self.handle_signal(ctx, bar, prior_high, prior_low);
        self.channel.push(bar);
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        let is_active_order = self
            .active_order
            .as_ref()
            .is_some_and(|order_id| order_id == &fill.order_id);
        if is_active_order && fill.side == Side::Buy && self.entry_target_raw.is_some() {
            self.entry_filled_raw = self
                .entry_filled_raw
                .saturating_add(fill.qty.raw());
        }
    }
}

export_strategy!(TenDayChannelTrend {
    channel: PriceChannel::new(),
    entry_target_raw: None,
    entry_filled_raw: 0,
    active_order: None,
    exiting: false,
});
