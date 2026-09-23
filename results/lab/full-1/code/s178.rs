use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, Fill, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const VOLUME_CAP_DENOMINATOR: i64 = 10;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct SeenBar {
    close_ts_ns: i64,
    close_raw: i64,
}

struct FundingWindowRebound {
    history: VecDeque<SeenBar>,
    entry_ts_ns: Option<i64>,
}

impl FundingWindowRebound {
    fn close_at(&self, ts_ns: i64) -> Option<i64> {
        self.history
            .iter()
            .rev()
            .find(|seen| seen.close_ts_ns == ts_ns)
            .map(|seen| seen.close_raw)
    }

    fn is_confirmation_bar(&self, ts_ns: i64, minute_ns: i64) -> bool {
        let minute_of_day = (ts_ns / minute_ns).rem_euclid(1440);
        matches!(minute_of_day, 5 | 485 | 965)
    }

    fn has_rebound_signal(&self, now_ns: i64, minute_ns: i64) -> bool {
        // At t+5, the required completed closes are at t-60, t-5, and t.
        let Some(close_60_before) = self.close_at(now_ns - 65 * minute_ns) else {
            return false;
        };
        let Some(close_5_before) = self.close_at(now_ns - 10 * minute_ns) else {
            return false;
        };
        let Some(close_at_settlement) = self.close_at(now_ns - 5 * minute_ns) else {
            return false;
        };

        if close_60_before <= 0 || close_at_settlement <= 0 {
            return false;
        }

        // First return <= -0.30%; post-settlement return >= +0.10%.
        i128::from(close_5_before) * 1_000
            <= i128::from(close_60_before) * 997
            && i128::from(self.history.back().map_or(0, |seen| seen.close_raw)) * 1_000
                >= i128::from(close_at_settlement) * 1_001
    }

    fn volume_limited_qty(volume: Qty) -> Qty {
        Qty::from_raw((volume.raw() / VOLUME_CAP_DENOMINATOR).max(0))
    }
}

impl Strategy for FundingWindowRebound {
    fn name(&self) -> String {
        "funding-window-rebound".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let now_ns = bar.close_ts.as_nanos();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        self.history.push_back(SeenBar {
            close_ts_ns: now_ns,
            close_raw: bar.close.raw(),
        });
        while self.history.len() > 70 {
            self.history.pop_front();
        }

        if ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            if let Some(entry_ns) = self.entry_ts_ns {
                let exit_ready_ns = entry_ns.saturating_add(TimeDelta::from_mins(120).as_nanos());
                // The order submitted at this close is eligible for the next bar's open.
                if now_ns >= exit_ready_ns {
                    let qty = position.raw().min(Self::volume_limited_qty(bar.volume).raw());
                    if qty > 0 {
                        ctx.submit(OrderRequest::market(
                            INSTRUMENT,
                            Side::Sell,
                            Qty::from_raw(qty),
                        ));
                    }
                }
            }
            return;
        }

        self.entry_ts_ns = None;
        if !self.is_confirmation_bar(now_ns, minute_ns)
            || !self.has_rebound_signal(now_ns, minute_ns)
        {
            return;
        }

        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        let qty = affordable
            .raw()
            .min(Self::volume_limited_qty(bar.volume).raw());
        if qty > 0 {
            ctx.submit(OrderRequest::market(
                INSTRUMENT,
                Side::Buy,
                Qty::from_raw(qty),
            ));
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Buy && self.entry_ts_ns.is_none() {
            self.entry_ts_ns = Some(fill.ts.as_nanos());
        }
    }
}

export_strategy!(FundingWindowRebound {
    history: VecDeque::with_capacity(70),
    entry_ts_ns: None,
});
