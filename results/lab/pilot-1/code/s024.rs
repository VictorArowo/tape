use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const LOOKBACK_DAYS: usize = 30;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct RangeExpansionHold {
    daily_ranges: VecDeque<f64>,
    entry_ts: Option<i64>,
}

impl RangeExpansionHold {
    fn median_previous_ranges(&self) -> Option<f64> {
        if self.daily_ranges.len() < LOOKBACK_DAYS {
            return None;
        }
        let mut values: Vec<f64> = self.daily_ranges.iter().copied().collect();
        values.sort_by(f64::total_cmp);
        Some((values[LOOKBACK_DAYS / 2 - 1] + values[LOOKBACK_DAYS / 2]) / 2.0)
    }
}

impl Strategy for RangeExpansionHold {
    fn name(&self) -> String {
        "range-expansion-hold".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        if ctx.open_orders() == 0 {
            if let Some(entry_ts) = self.entry_ts {
                if ctx.position(INSTRUMENT).is_positive()
                    && ctx.now().as_nanos() - entry_ts >= TimeDelta::from_hours(24).as_nanos()
                {
                    let position = ctx.position(INSTRUMENT);
                    if position.is_positive() {
                        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                    }
                }
            }
        }

        let minute = (bar.close_ts.as_nanos() / TimeDelta::from_mins(1).as_nanos()) % 1440;
        if minute != 1439 {
            return;
        }

        let open = bar.open.to_f64();
        let high = bar.high.to_f64();
        let low = bar.low.to_f64();
        let close = bar.close.to_f64();
        let span = high - low;
        if open <= 0.0 || span <= 0.0 {
            return;
        }

        let normalized_range = span / open;
        let close_location = (close - low) / span;
        let median = self.median_previous_ranges();
        let qualifies = median.is_some_and(|previous_median| {
            normalized_range > previous_median && close_location >= 0.8
        });

        if qualifies
            && ctx.open_orders() == 0
            && ctx.position(INSTRUMENT).is_zero()
        {
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        }

        self.daily_ranges.push_back(normalized_range);
        while self.daily_ranges.len() > LOOKBACK_DAYS {
            self.daily_ranges.pop_front();
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        match fill.side {
            Side::Buy => self.entry_ts = Some(fill.ts.as_nanos()),
            Side::Sell => self.entry_ts = None,
        }
    }
}

export_strategy!(RangeExpansionHold {
    daily_ranges: VecDeque::with_capacity(LOOKBACK_DAYS),
    entry_ts: None,
});