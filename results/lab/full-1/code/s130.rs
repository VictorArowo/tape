use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const LOOKBACK_DAYS: i64 = 28;
const WINDOW_BARS: usize = 30;

#[derive(Clone, Copy)]
struct Sample {
    close_ts: i64,
    close_raw: i64,
    volume_raw: i64,
}

#[derive(Clone, Copy)]
struct VolumeObservation {
    day: i64,
    slot: u8,
    volume: i128,
}

struct FundingSettlementRebound {
    bars: VecDeque<Sample>,
    observations: VecDeque<VolumeObservation>,
    entry_ts: Option<i64>,
}

impl FundingSettlementRebound {
    fn new() -> Self {
        Self {
            bars: VecDeque::with_capacity(WINDOW_BARS + 1),
            observations: VecDeque::with_capacity(90),
            entry_ts: None,
        }
    }

    fn median_pair(&self, day: i64, slot: u8) -> Option<(i128, i128)> {
        let first_day = day - LOOKBACK_DAYS;
        let mut values: Vec<i128> = self
            .observations
            .iter()
            .filter(|item| item.slot == slot && item.day >= first_day && item.day < day)
            .map(|item| item.volume)
            .collect();

        // Require one valid observation for each of the preceding 28 calendar days.
        if values.len() != LOOKBACK_DAYS as usize {
            return None;
        }
        values.sort_unstable();
        Some((values[13], values[14]))
    }
}

impl Strategy for FundingSettlementRebound {
    fn name(&self) -> String {
        String::from("Funding-Settlement Rebound")
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let close_ns = bar.close_ts.as_nanos();
        let total_minutes = close_ns.div_euclid(minute_ns);
        let minute_of_day = total_minutes.rem_euclid(1440);
        let day = close_ns.div_euclid(day_ns);

        self.bars.push_back(Sample {
            close_ts: close_ns,
            close_raw: bar.close.raw(),
            volume_raw: bar.volume.raw(),
        });
        while self.bars.len() > WINDOW_BARS + 1 {
            self.bars.pop_front();
        }

        let slot = match minute_of_day {
            0 => Some(0_u8),
            480 => Some(1_u8),
            960 => Some(2_u8),
            _ => None,
        };

        let mut entry_signal = false;
        if let Some(slot) = slot {
            let already_recorded = self
                .observations
                .iter()
                .any(|item| item.day == day && item.slot == slot);

            if !already_recorded && self.bars.len() == WINDOW_BARS + 1 {
                let contiguous = self
                    .bars
                    .iter()
                    .zip(self.bars.iter().skip(1))
                    .all(|(previous, next)| next.close_ts - previous.close_ts == minute_ns);

                if contiguous {
                    let first_close = self.bars.front().map(|sample| sample.close_raw);
                    let last_close = self.bars.back().map(|sample| sample.close_raw);
                    let volume: i128 = self
                        .bars
                        .iter()
                        .skip(1)
                        .map(|sample| i128::from(sample.volume_raw))
                        .sum();

                    if let (Some(first_close), Some(last_close)) = (first_close, last_close) {
                        if first_close > 0 && last_close > 0 {
                            if let Some((middle_low, middle_high)) = self.median_pair(day, slot) {
                                // Even-sized median is (middle_low + middle_high) / 2.
                                // Compare exactly against 1.5 times that median.
                                let volume_is_elevated = 4 * volume >= 3 * (middle_low + middle_high);
                                let return_is_low = i128::from(last_close) * 1_000
                                    <= i128::from(first_close) * 995;
                                entry_signal = return_is_low && volume_is_elevated;
                            }
                        }
                    }

                    self.observations.push_back(VolumeObservation { day, slot, volume });
                    let oldest_kept_day = day - LOOKBACK_DAYS;
                    while self
                        .observations
                        .front()
                        .is_some_and(|item| item.day < oldest_kept_day)
                    {
                        self.observations.pop_front();
                    }
                }
            }
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_zero() {
            self.entry_ts = None;
        } else if position.is_positive() {
            if let Some(entry_ts) = self.entry_ts {
                let exit_due = entry_ts + TimeDelta::from_mins(90).as_nanos();
                if close_ns >= exit_due && ctx.open_orders() == 0 {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                    return;
                }
            }
            return;
        } else {
            return;
        }

        if !entry_signal || ctx.open_orders() != 0 || !bar.close.is_positive() {
            return;
        }

        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        // The execution bar's volume is not available until it closes. Bound the
        // request using the latest completed bar; the simulator must enforce any
        // execution-bar participation cap itself.
        let volume_cap = bar.volume.mul_ratio(1, 10);
        let qty = Qty::from_raw(affordable.raw().min(volume_cap.raw()));
        if qty.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        match fill.side {
            Side::Buy => {
                if self.entry_ts.is_none() {
                    self.entry_ts = Some(fill.ts.as_nanos());
                }
            }
            Side::Sell => {
                if ctx.position(INSTRUMENT).is_zero() {
                    self.entry_ts = None;
                }
            }
        }
    }

    fn on_finish(&mut self, ctx: &mut Ctx) {
        let position = ctx.position(INSTRUMENT);
        if position.is_positive() && ctx.open_orders() == 0 {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
        }
    }
}

export_strategy!(FundingSettlementRebound::new());
