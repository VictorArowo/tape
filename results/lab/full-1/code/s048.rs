use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const HISTORY_HOURS: usize = 24;

fn hour_ns() -> i64 {
    TimeDelta::from_hours(1).as_nanos()
}

#[derive(Clone, Copy)]
struct HourBar {
    high: i64,
    low: i64,
    close: i64,
    volume: i128,
    close_ns: i64,
}

struct BuildingHour {
    key: i64,
    high: i64,
    low: i64,
    close: i64,
    volume: i128,
    close_ns: i64,
}

#[derive(Clone, Copy)]
struct Setup {
    level: i64,
    atr_sum: i128, // 24 * ATR, in raw-price units
    elapsed_hours: u8,
}

#[derive(Clone, Copy)]
struct PendingEntry {
    level: i64,
    atr_sum: i128,
    submitted_ns: i64,
}

struct PositionState {
    level: i64,
    atr_sum: i128,
    entry_ns: i64,
    highest_close: Option<i64>,
    exit_sent: bool,
}

struct RangeAcceptanceRetest {
    building_hour: Option<BuildingHour>,
    history: VecDeque<HourBar>,
    setup: Option<Setup>,
    pending_entry: Option<PendingEntry>,
    position: Option<PositionState>,
}

impl RangeAcceptanceRetest {
    fn new() -> Self {
        Self {
            building_hour: None,
            history: VecDeque::with_capacity(HISTORY_HOURS + 2),
            setup: None,
            pending_entry: None,
            position: None,
        }
    }

    fn finish_building_hour(&mut self, ctx: &mut Ctx) {
        if let Some(hour) = self.building_hour.take() {
            let completed = HourBar {
                high: hour.high,
                low: hour.low,
                close: hour.close,
                volume: hour.volume,
                close_ns: hour.close_ns,
            };
            self.on_hour_close(ctx, completed);
        }
    }

    fn preceding_stats(&self) -> Option<(i64, i128, i128)> {
        // One extra bar supplies the prior close needed for the first true range.
        if self.history.len() < HISTORY_HOURS + 1 {
            return None;
        }

        let start = self.history.len() - HISTORY_HOURS;
        let mut highest = i64::MIN;
        let mut volumes = Vec::with_capacity(HISTORY_HOURS);
        let mut atr_sum = 0i128;

        for index in start..self.history.len() {
            let current = &self.history[index];
            let previous = &self.history[index - 1];
            highest = highest.max(current.high);
            volumes.push(current.volume);

            let high = i128::from(current.high);
            let low = i128::from(current.low);
            let previous_close = i128::from(previous.close);
            let true_range = (high - low)
                .max((high - previous_close).abs())
                .max((low - previous_close).abs());
            atr_sum += true_range;
        }

        if atr_sum <= 0 {
            return None;
        }

        volumes.sort_unstable();
        // With 24 observations the median is the average of the two middle values.
        let median_middle_sum = volumes[11] + volumes[12];
        Some((highest, atr_sum, median_middle_sum))
    }

    fn is_breakout(hour: &HourBar, level: i64, atr_sum: i128, median_middle_sum: i128) -> bool {
        // close > H + 0.25 * ATR, where ATR = atr_sum / 24.
        let close_delta = i128::from(hour.close) - i128::from(level);
        let price_breakout = 96 * close_delta > atr_sum;
        // Current volume > 1.5 * median; median = middle_sum / 2.
        let volume_breakout = 4 * hour.volume > 3 * median_middle_sum;
        price_breakout && volume_breakout
    }

    fn is_retest(hour: &HourBar, setup: Setup) -> bool {
        let low_delta = i128::from(hour.low) - i128::from(setup.level);
        let close_at_or_above_level = hour.close >= setup.level;
        // H - 0.25 ATR <= low <= H + 0.10 ATR.
        let above_lower_bound = 96 * low_delta >= -setup.atr_sum;
        let below_upper_bound = 240 * low_delta <= setup.atr_sum;
        close_at_or_above_level && above_lower_bound && below_upper_bound
    }

    fn submit_entry(&mut self, ctx: &mut Ctx, hour: &HourBar, setup: Setup) {
        if ctx.open_orders() != 0 || ctx.position(INSTRUMENT).is_positive() {
            return;
        }

        let price = Price::from_raw(hour.close);
        if !price.is_positive() {
            return;
        }
        let budget = ctx.cash().mul_ratio(95, 100);
        let qty = ctx.affordable_qty(INSTRUMENT, price, budget, SIZING_MARGIN_PPM);
        if !qty.is_positive() {
            return;
        }

        self.pending_entry = Some(PendingEntry {
            level: setup.level,
            atr_sum: setup.atr_sum,
            submitted_ns: hour.close_ns,
        });
        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
    }

    fn manage_position(&mut self, ctx: &mut Ctx, hour: &HourBar) {
        let Some(position_state) = self.position.as_mut() else {
            return;
        };
        if position_state.exit_sent || hour.close_ns < position_state.entry_ns {
            return;
        }

        let highest = position_state
            .highest_close
            .map_or(hour.close, |previous| previous.max(hour.close));
        position_state.highest_close = Some(highest);

        let close_delta_from_level = i128::from(hour.close) - i128::from(position_state.level);
        let below_level_stop = 48 * close_delta_from_level < -position_state.atr_sum;

        let close_delta_from_highest = i128::from(hour.close) - i128::from(highest);
        let below_trailing_stop = 12 * close_delta_from_highest < -position_state.atr_sum;

        let max_holding_ns = 72 * hour_ns();
        let time_stop = hour.close_ns
            >= position_state.entry_ns.saturating_add(max_holding_ns);

        if (below_level_stop || below_trailing_stop || time_stop)
            && ctx.open_orders() == 0
        {
            let qty = ctx.position(INSTRUMENT);
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
                position_state.exit_sent = true;
            }
        }
    }

    fn on_hour_close(&mut self, ctx: &mut Ctx, hour: HourBar) {
        let current_position = ctx.position(INSTRUMENT);

        if current_position.is_positive() {
            self.setup = None;
            if self.position.is_none() {
                let pending = self.pending_entry.take();
                self.position = Some(PositionState {
                    level: pending.map_or(hour.close, |entry| entry.level),
                    atr_sum: pending.map_or(0, |entry| entry.atr_sum),
                    entry_ns: pending.map_or(hour.close_ns, |entry| entry.submitted_ns),
                    highest_close: None,
                    exit_sent: false,
                });
            }
            self.manage_position(ctx, &hour);
        } else {
            self.position = None;

            if self.pending_entry.is_none() {
                let mut consumed_setup = false;
                if let Some(mut setup) = self.setup.take() {
                    setup.elapsed_hours = setup.elapsed_hours.saturating_add(1);
                    if Self::is_retest(&hour, setup) {
                        consumed_setup = true;
                        self.submit_entry(ctx, &hour, setup);
                    } else if setup.elapsed_hours < 24 {
                        self.setup = Some(setup);
                    }
                }

                if !consumed_setup && self.setup.is_none() {
                    if let Some((level, atr_sum, median_middle_sum)) = self.preceding_stats() {
                        if Self::is_breakout(&hour, level, atr_sum, median_middle_sum) {
                            self.setup = Some(Setup {
                                level,
                                atr_sum,
                                elapsed_hours: 0,
                            });
                        }
                    }
                }
            }
        }

        self.history.push_back(hour);
        while self.history.len() > HISTORY_HOURS + 1 {
            self.history.pop_front();
        }
    }
}

impl Strategy for RangeAcceptanceRetest {
    fn name(&self) -> String {
        "Range-Acceptance Retest".to_owned()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        // Recover or discard an IOC entry intent once its next-bar attempt has passed.
        if let Some(pending) = self.pending_entry {
            if ctx.position(INSTRUMENT).is_positive() {
                if self.position.is_none() {
                    self.position = Some(PositionState {
                        level: pending.level,
                        atr_sum: pending.atr_sum,
                        entry_ns: pending.submitted_ns,
                        highest_close: None,
                        exit_sent: false,
                    });
                }
                self.pending_entry = None;
            } else if bar.close_ts.as_nanos() > pending.submitted_ns && ctx.open_orders() == 0 {
                // Any unfilled IOC remainder is deliberately not resubmitted.
                self.pending_entry = None;
            }
        }

        let hour_length = hour_ns();
        let key = bar.open_ts.as_nanos().div_euclid(hour_length);

        // A timestamp gap can reveal the prior hour's completion as well.
        if self
            .building_hour
            .as_ref()
            .is_some_and(|building| building.key != key)
        {
            self.finish_building_hour(ctx);
        }

        let high = bar.high.raw();
        let low = bar.low.raw();
        let close = bar.close.raw();
        let volume = i128::from(bar.volume.raw());

        match self.building_hour.as_mut() {
            Some(building) => {
                building.high = building.high.max(high);
                building.low = building.low.min(low);
                building.close = close;
                building.volume += volume;
                building.close_ns = bar.close_ts.as_nanos();
            }
            None => {
                self.building_hour = Some(BuildingHour {
                    key,
                    high,
                    low,
                    close,
                    volume,
                    close_ns: bar.close_ts.as_nanos(),
                });
            }
        }

        // Minute bars are assigned by open time; a close crossing the UTC hour
        // boundary completes the aggregate and submits any resulting order now,
        // for execution at the next one-minute bar's open.
        if bar.close_ts.as_nanos().div_euclid(hour_length) > key {
            self.finish_building_hour(ctx);
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.side == Side::Buy {
            if self.position.is_none() {
                if let Some(pending) = self.pending_entry.take() {
                    self.position = Some(PositionState {
                        level: pending.level,
                        atr_sum: pending.atr_sum,
                        entry_ns: fill.ts.as_nanos(),
                        highest_close: None,
                        exit_sent: false,
                    });
                }
            }
        } else if fill.side == Side::Sell && ctx.position(INSTRUMENT).is_zero() {
            self.position = None;
            self.setup = None;
        }
    }
}

export_strategy!(RangeAcceptanceRetest::new());
