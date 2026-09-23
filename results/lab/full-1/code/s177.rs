use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, Fill, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta,
    TimeInForce, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const MINUTE_NS: i64 = TimeDelta::from_mins(1).as_nanos();
const DAY_MINUTES: i64 = 1440;
const LOOKBACK_DAYS: i64 = 7;
const BASELINE_WINDOWS: usize = 21;
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy)]
struct MinuteBar {
    minute: i64,
    open_raw: i64,
    close_raw: i64,
    volume_raw: i64,
}

#[derive(Clone, Copy)]
struct WindowVolume {
    minute: i64,
    total_raw: i128,
}

#[derive(Clone, Copy)]
struct Candidate {
    settlement_minute: i64,
    pre_settlement_close_raw: i64,
}

struct FundingClockReliefBounce {
    bars: VecDeque<MinuteBar>,
    window_volumes: VecDeque<WindowVolume>,
    candidate: Option<Candidate>,
    entry_minute: Option<i64>,
}

impl FundingClockReliefBounce {
    fn is_funding_minute(minute: i64) -> bool {
        matches!(minute.rem_euclid(DAY_MINUTES), 0 | 480 | 960)
    }

    fn current_window(&self, minute: i64) -> Option<(i64, i64, i128)> {
        if self.bars.len() != 30 {
            return None;
        }

        let first = *self.bars.front()?;
        let last = *self.bars.back()?;
        for (index, bar) in self.bars.iter().enumerate() {
            let expected = minute - (29 - index as i64);
            if bar.minute != expected {
                return None;
            }
        }

        let total_volume = self
            .bars
            .iter()
            .map(|bar| i128::from(bar.volume_raw))
            .sum();
        Some((first.open_raw, last.close_raw, total_volume))
    }

    fn handle_funding_minute(&mut self, ctx: &Ctx, minute: i64) {
        self.candidate = None;

        let cutoff = minute - LOOKBACK_DAYS * DAY_MINUTES;
        while self
            .window_volumes
            .front()
            .is_some_and(|window| window.minute < cutoff)
        {
            self.window_volumes.pop_front();
        }

        let Some((first_open_raw, last_close_raw, total_volume_raw)) =
            self.current_window(minute)
        else {
            return;
        };

        let mut baseline: Vec<i128> = self
            .window_volumes
            .iter()
            .filter(|window| window.minute >= cutoff && window.minute < minute)
            .map(|window| window.total_raw)
            .collect();
        baseline.sort_unstable();

        let high_volume = if baseline.len() == BASELINE_WINDOWS {
            total_volume_raw * 2 >= baseline[BASELINE_WINDOWS / 2] * 3
        } else {
            false
        };

        let declined = i128::from(last_close_raw) * 10_000
            <= i128::from(first_open_raw) * 9_960;

        self.window_volumes.push_back(WindowVolume {
            minute,
            total_raw: total_volume_raw,
        });

        if high_volume && declined && ctx.position(INSTRUMENT).is_zero() {
            self.candidate = Some(Candidate {
                settlement_minute: minute,
                pre_settlement_close_raw: last_close_raw,
            });
        }
    }

    fn capped_qty(wanted: Qty, volume: Qty) -> Qty {
        let cap_raw = volume.raw() / 10;
        Qty::from_raw(wanted.raw().min(cap_raw).max(0))
    }

    fn try_exit(&self, ctx: &mut Ctx, bar: &Bar, minute: i64) {
        let position = ctx.position(INSTRUMENT);
        let Some(entry_minute) = self.entry_minute else {
            return;
        };
        if position.is_zero()
            || minute < entry_minute + 120
            || ctx.open_orders() != 0
        {
            return;
        }

        let qty = Self::capped_qty(position, bar.volume);
        if qty.is_positive() {
            ctx.submit(
                OrderRequest::market(INSTRUMENT, Side::Sell, qty)
                    .with_time_in_force(TimeInForce::Ioc),
            );
        }
    }
}

impl Strategy for FundingClockReliefBounce {
    fn name(&self) -> String {
        "Funding-Clock Relief Bounce".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let minute = bar.close_ts.as_nanos().div_euclid(MINUTE_NS);
        if self
            .bars
            .back()
            .is_some_and(|previous| previous.minute + 1 != minute)
        {
            self.bars.clear();
        }
        self.bars.push_back(MinuteBar {
            minute,
            open_raw: bar.open.raw(),
            close_raw: bar.close.raw(),
            volume_raw: bar.volume.raw(),
        });
        while self.bars.len() > 30 {
            self.bars.pop_front();
        }

        if self
            .candidate
            .is_some_and(|candidate| minute > candidate.settlement_minute + 15)
        {
            self.candidate = None;
        }

        if Self::is_funding_minute(minute) {
            self.handle_funding_minute(ctx, minute);
        }

        if ctx.position(INSTRUMENT).is_zero()
            && ctx.open_orders() == 0
            && self.entry_minute.is_some()
        {
            self.entry_minute = None;
        }

        self.try_exit(ctx, bar, minute);

        let Some(candidate) = self.candidate else {
            return;
        };
        if minute != candidate.settlement_minute + 15 {
            return;
        }
        self.candidate = None;

        if i128::from(bar.close.raw()) * 100_000
            < i128::from(candidate.pre_settlement_close_raw) * 100_150
            || !ctx.position(INSTRUMENT).is_zero()
            || ctx.open_orders() != 0
        {
            return;
        }

        // Budget with a small price allowance for slippage; affordability also
        // accounts for fees using the SDK's supplied margin buffer.
        let sizing_price = bar.close.mul_ratio(10_002, 10_000);
        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            sizing_price,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        let qty = Self::capped_qty(affordable, bar.volume);
        if qty.is_positive() {
            ctx.submit(
                OrderRequest::market(INSTRUMENT, Side::Buy, qty)
                    .with_time_in_force(TimeInForce::Ioc),
            );
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Buy && self.entry_minute.is_none() {
            self.entry_minute = Some(fill.ts.as_nanos().div_euclid(MINUTE_NS));
        }
    }
}

export_strategy!(FundingClockReliefBounce {
    bars: VecDeque::with_capacity(30),
    window_volumes: VecDeque::with_capacity(BASELINE_WINDOWS + 1),
    candidate: None,
    entry_minute: None,
});
