use std::collections::BTreeMap;

use tape_guest::{
    Bar, Ctx, Fill, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const MINUTES_PER_DAY: i64 = 1440;
const SETTLEMENTS: [i64; 3] = [0, 480, 960];
const HISTORY_MINUTES: i64 = 15 * MINUTES_PER_DAY;
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy)]
struct Record {
    close_raw: i64,
    volume_raw: i64,
}

#[derive(Clone, Copy)]
struct Setup {
    settlement_minute: i64,
    older_close_raw: i64,
    presettlement_close_raw: i64,
}

struct FundingWindowInventoryReversal {
    history: BTreeMap<i64, Record>,
    setup: Option<Setup>,
    entry_campaign: bool,
    entry_target: Qty,
    entry_rebound_sum_raw: i128,
    exit_active: bool,
    entry_notional_raw: i128,
    entry_qty_raw: i128,
    first_entry_minute: Option<i64>,
    last_held_bar_minute: Option<i64>,
    held_bars: u32,
}

impl FundingWindowInventoryReversal {
    fn new() -> Self {
        Self {
            history: BTreeMap::new(),
            setup: None,
            entry_campaign: false,
            entry_target: Qty::from_raw(0),
            entry_rebound_sum_raw: 0,
            exit_active: false,
            entry_notional_raw: 0,
            entry_qty_raw: 0,
            first_entry_minute: None,
            last_held_bar_minute: None,
            held_bars: 0,
        }
    }

    fn minute_index(ts: tape_guest::Timestamp) -> i64 {
        ts.as_nanos()
            .div_euclid(TimeDelta::from_mins(1).as_nanos())
    }

    fn window_volume(&self, settlement_minute: i64) -> Option<i128> {
        let mut total = 0_i128;
        for offset in 1_i64..=30 {
            let record = self.history.get(&(settlement_minute - offset))?;
            total += i128::from(record.volume_raw);
        }
        Some(total)
    }

    fn qualifying_setup(&self, settlement_minute: i64) -> Option<Setup> {
        let older_close = self.history.get(&(settlement_minute - 31))?.close_raw;
        let presettlement_close = self.history.get(&(settlement_minute - 1))?.close_raw;
        if older_close <= 0
            || i128::from(presettlement_close) * 1_000
                > i128::from(older_close) * 994
        {
            return None;
        }

        let current_volume = self.window_volume(settlement_minute)?;
        let mut historical_volumes = Vec::with_capacity(14);
        for days_ago in 1_i64..=14 {
            let prior_settlement = settlement_minute - days_ago * MINUTES_PER_DAY;
            historical_volumes.push(self.window_volume(prior_settlement)?);
        }
        historical_volumes.sort_unstable();

        // For 14 observations, the median is the mean of the two middle values.
        // This comparison is equivalent to current_volume >= 1.5 * median.
        let middle_sum = historical_volumes[6] + historical_volumes[7];
        if 4 * current_volume < 3 * middle_sum {
            return None;
        }

        Some(Setup {
            settlement_minute,
            older_close_raw: older_close,
            presettlement_close_raw: presettlement_close,
        })
    }

    fn try_entry(&self, ctx: &mut Ctx, bar: &Bar) {
        if !self.entry_campaign || ctx.open_orders() != 0 {
            return;
        }
        let position_raw = ctx.position(INSTRUMENT).raw();
        let remaining_raw = self.entry_target.raw().saturating_sub(position_raw);
        if remaining_raw <= 0 {
            return;
        }

        let cap_raw = bar.volume.mul_ratio(1, 10).raw();
        let affordable_raw = ctx
            .affordable_qty(INSTRUMENT, bar.close, ctx.cash(), SIZING_MARGIN_PPM)
            .raw();
        let qty_raw = remaining_raw.min(cap_raw).min(affordable_raw);
        if qty_raw > 0 {
            ctx.submit(OrderRequest::market(
                INSTRUMENT,
                Side::Buy,
                Qty::from_raw(qty_raw),
            ));
        }
    }

    fn try_exit(&self, ctx: &mut Ctx, bar: &Bar) {
        if !self.exit_active || ctx.open_orders() != 0 {
            return;
        }
        let position = ctx.position(INSTRUMENT);
        if !position.is_positive() {
            return;
        }
        let cap_raw = bar.volume.mul_ratio(1, 10).raw();
        let qty_raw = position.raw().min(cap_raw);
        if qty_raw > 0 {
            ctx.submit(OrderRequest::market(
                INSTRUMENT,
                Side::Sell,
                Qty::from_raw(qty_raw),
            ));
        }
    }

    fn clear_position_tracking(&mut self) {
        self.exit_active = false;
        self.entry_notional_raw = 0;
        self.entry_qty_raw = 0;
        self.first_entry_minute = None;
        self.last_held_bar_minute = None;
        self.held_bars = 0;
    }
}

impl Strategy for FundingWindowInventoryReversal {
    fn name(&self) -> String {
        "funding-window-inventory-reversal".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let minute = Self::minute_index(bar.close_ts);
        self.history.insert(
            minute,
            Record {
                close_raw: bar.close.raw(),
                volume_raw: bar.volume.raw(),
            },
        );
        let oldest_kept = minute - HISTORY_MINUTES;
        self.history.retain(|key, _| *key >= oldest_kept);

        let mut position = ctx.position(INSTRUMENT);
        if position.is_zero() && self.exit_active {
            self.clear_position_tracking();
        }

        if let Some(setup) = self.setup {
            let observation_end = setup.settlement_minute + 14;
            if minute == observation_end {
                self.setup = None;
                let rebound_sum = i128::from(setup.older_close_raw)
                    + i128::from(setup.presettlement_close_raw);
                if position.is_zero()
                    && !self.entry_campaign
                    && !self.exit_active
                    && i128::from(bar.close.raw()) * 2 >= rebound_sum
                {
                    let target = ctx.affordable_qty(
                        INSTRUMENT,
                        bar.close,
                        ctx.cash(),
                        SIZING_MARGIN_PPM,
                    );
                    if target.is_positive() {
                        self.entry_campaign = true;
                        self.entry_target = target;
                        self.entry_rebound_sum_raw = rebound_sum;
                    }
                }
            } else if minute > observation_end {
                self.setup = None;
            }
        }

        let minute_of_day = minute.rem_euclid(MINUTES_PER_DAY);
        if SETTLEMENTS.contains(&minute_of_day)
            && position.is_zero()
            && !self.entry_campaign
            && !self.exit_active
            && self.setup.is_none()
        {
            if let Some(setup) = self.qualifying_setup(minute) {
                self.setup = Some(setup);
            }
        }

        position = ctx.position(INSTRUMENT);
        if self.entry_campaign {
            if i128::from(bar.close.raw()) * 2 < self.entry_rebound_sum_raw {
                // The confirmed rebound has failed; stop carrying any entry remainder.
                self.entry_campaign = false;
                self.entry_target = Qty::from_raw(0);
            } else if position.raw() >= self.entry_target.raw() {
                self.entry_campaign = false;
            }
        }

        if position.is_positive() {
            if let Some(entry_minute) = self.first_entry_minute {
                if minute >= entry_minute && self.last_held_bar_minute != Some(minute) {
                    self.held_bars = self.held_bars.saturating_add(1);
                    self.last_held_bar_minute = Some(minute);
                }
            }

            if !self.exit_active && self.entry_qty_raw > 0 {
                let average_entry_raw = self.entry_notional_raw / self.entry_qty_raw;
                let close_raw = i128::from(bar.close.raw());
                let stop_hit = close_raw * 1_000 <= average_entry_raw * 992;
                let target_hit = close_raw * 1_000 >= average_entry_raw * 1_016;
                if stop_hit || target_hit || self.held_bars >= 120 {
                    self.exit_active = true;
                    self.entry_campaign = false;
                    self.entry_target = Qty::from_raw(0);
                }
            }

            if self.exit_active {
                self.try_exit(ctx, bar);
                return;
            }
        }

        if self.entry_campaign {
            self.try_entry(ctx, bar);
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Buy && fill.qty.is_positive() {
            let qty_raw = i128::from(fill.qty.raw());
            self.entry_notional_raw += i128::from(fill.price.raw()) * qty_raw;
            self.entry_qty_raw += qty_raw;
            if self.first_entry_minute.is_none() {
                self.first_entry_minute = Some(Self::minute_index(fill.ts));
            }
        }
    }
}

export_strategy!(FundingWindowInventoryReversal::new());
