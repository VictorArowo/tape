use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const BLOCK_MINUTES: i64 = 240;
const PRIOR_BLOCKS: usize = 180;
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy)]
enum Mode {
    Initial,
    EnteringInitial,
    Holding,
    Cooldown { since_ns: i64, high_raw: i64 },
    EnteringReentry { since_ns: i64, high_raw: i64 },
}

struct UpsideBurstCooldown {
    mode: Mode,
    accumulating_block: Option<i64>,
    block_volume_raw: i128,
    block_bar_count: u16,
    prior_volumes: VecDeque<i128>,
    last_completed: Option<(i64, i64)>,
}

impl UpsideBurstCooldown {
    fn new() -> Self {
        Self {
            mode: Mode::Initial,
            accumulating_block: None,
            block_volume_raw: 0,
            block_bar_count: 0,
            prior_volumes: VecDeque::with_capacity(PRIOR_BLOCKS),
            last_completed: None,
        }
    }

    fn place_buy(ctx: &mut Ctx, price: Price) -> bool {
        if ctx.open_orders() != 0 {
            return false;
        }
        let qty = ctx.affordable_qty(INSTRUMENT, price, ctx.cash(), SIZING_MARGIN_PPM);
        if !qty.is_positive() {
            return false;
        }
        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        true
    }

    fn place_sell_all(ctx: &mut Ctx) -> bool {
        if ctx.open_orders() != 0 {
            return false;
        }
        let qty = ctx.position(INSTRUMENT);
        if !qty.is_positive() {
            return false;
        }
        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
        true
    }

    fn update_cooldown_high(&mut self, close: Price) {
        let raw = close.raw();
        match &mut self.mode {
            Mode::Cooldown { high_raw, .. }
            | Mode::EnteringReentry { high_raw, .. } => {
                *high_raw = (*high_raw).max(raw);
            }
            _ => {}
        }
    }

    fn should_reenter(since_ns: i64, high_raw: i64, bar: &Bar) -> bool {
        let elapsed = bar.close_ts.as_nanos().saturating_sub(since_ns);
        let six_hours = TimeDelta::from_hours(6).as_nanos();
        let thirty_six_hours = TimeDelta::from_hours(36).as_nanos();
        let close_raw = i128::from(bar.close.raw());
        let below_high = close_raw * 1_000 <= i128::from(high_raw) * 975;
        (elapsed >= six_hours && below_high) || elapsed >= thirty_six_hours
    }

    fn finish_entry_if_funded(&mut self, ctx: &Ctx, price: Price) {
        let entering = matches!(self.mode, Mode::EnteringInitial | Mode::EnteringReentry { .. });
        if entering
            && ctx.open_orders() == 0
            && ctx.position(INSTRUMENT).is_positive()
            && !ctx
                .affordable_qty(INSTRUMENT, price, ctx.cash(), SIZING_MARGIN_PPM)
                .is_positive()
        {
            self.mode = Mode::Holding;
        }
    }

    // Returns true only for a complete 4-hour block meeting the burst rule.
    fn observe_bar(&mut self, bar: &Bar, invested: bool) -> bool {
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let minute_index = bar.close_ts.as_nanos() / minute_ns;
        // A bar closing exactly on a UTC boundary belongs to the block that just ended.
        let block_id = (minute_index - 1).div_euclid(BLOCK_MINUTES);

        if self.accumulating_block != Some(block_id) {
            self.accumulating_block = Some(block_id);
            self.block_volume_raw = 0;
            self.block_bar_count = 0;
        }
        self.block_volume_raw += i128::from(bar.volume.raw());
        self.block_bar_count = self.block_bar_count.saturating_add(1);

        let at_boundary = minute_index.rem_euclid(BLOCK_MINUTES) == 0;
        if !at_boundary {
            return false;
        }

        let complete = self.block_bar_count == BLOCK_MINUTES as u16;
        let volume_raw = self.block_volume_raw;
        self.accumulating_block = None;
        self.block_volume_raw = 0;
        self.block_bar_count = 0;
        if !complete {
            return false;
        }

        let mut burst = false;
        if invested && self.prior_volumes.len() == PRIOR_BLOCKS {
            if let Some((previous_id, previous_close_raw)) = self.last_completed {
                let adjacent = previous_id.saturating_add(1) == block_id;
                let price_advance = i128::from(bar.close.raw()) * 100
                    >= i128::from(previous_close_raw) * 103;

                let mut sorted: Vec<i128> = self.prior_volumes.iter().copied().collect();
                sorted.sort_unstable();
                let middle_pair_sum = sorted[89] + sorted[90];
                // For 180 samples the median is the average of the two middle values.
                // This comparison keeps that median exact, including half-raw-unit cases.
                let high_volume = volume_raw * 4 > middle_pair_sum * 3;
                burst = adjacent && price_advance && high_volume;
            }
        }

        self.last_completed = Some((block_id, bar.close.raw()));
        self.prior_volumes.push_back(volume_raw);
        if self.prior_volumes.len() > PRIOR_BLOCKS {
            self.prior_volumes.pop_front();
        }
        burst
    }
}

impl Strategy for UpsideBurstCooldown {
    fn name(&self) -> String {
        "upside-burst-cooldown".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        self.finish_entry_if_funded(ctx, bar.close);
        self.update_cooldown_high(bar.close);

        let invested = matches!(self.mode, Mode::Holding)
            && ctx.position(INSTRUMENT).is_positive();
        let burst = self.observe_bar(bar, invested);

        if burst && matches!(self.mode, Mode::Holding) {
            self.mode = Mode::Cooldown {
                since_ns: bar.close_ts.as_nanos(),
                high_raw: bar.close.raw(),
            };
            Self::place_sell_all(ctx);
            return;
        }

        match self.mode {
            Mode::Initial => {
                if Self::place_buy(ctx, bar.close) {
                    self.mode = Mode::EnteringInitial;
                }
            }
            Mode::EnteringInitial => {
                if ctx.open_orders() == 0 {
                    if !Self::place_buy(ctx, bar.close)
                        && ctx.position(INSTRUMENT).is_positive()
                    {
                        self.mode = Mode::Holding;
                    }
                }
            }
            Mode::Holding => {}
            Mode::Cooldown { since_ns, high_raw } => {
                if ctx.position(INSTRUMENT).is_positive() {
                    Self::place_sell_all(ctx);
                } else if Self::should_reenter(since_ns, high_raw, bar)
                    && Self::place_buy(ctx, bar.close)
                {
                    self.mode = Mode::EnteringReentry { since_ns, high_raw };
                }
            }
            Mode::EnteringReentry { since_ns, high_raw } => {
                if ctx.open_orders() == 0 {
                    if !Self::place_buy(ctx, bar.close)
                        && ctx.position(INSTRUMENT).is_positive()
                    {
                        self.mode = Mode::Holding;
                    } else if ctx.position(INSTRUMENT).is_zero()
                        && Self::should_reenter(since_ns, high_raw, bar)
                    {
                        // Keep the cooldown state if no buy could be submitted, so it can retry.
                        self.mode = Mode::Cooldown { since_ns, high_raw };
                    }
                }
            }
        }
    }
}

export_strategy!(UpsideBurstCooldown::new());
