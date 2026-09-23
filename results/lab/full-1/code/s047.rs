use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, Fill, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const MINUTES_PER_FOUR_HOURS: i64 = 240;
const SEVEN_DAY_BARS: usize = 42;
const THIRTY_DAY_BARS: usize = 180;
const HISTORY_LEN: usize = THIRTY_DAY_BARS + 1;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct FourHourBuilder {
    bucket: Option<i64>,
    count: usize,
    first_minute: i64,
    last_minute: i64,
    valid: bool,
    close: tape_guest::Price,
}

impl FourHourBuilder {
    fn new() -> Self {
        Self {
            bucket: None,
            count: 0,
            first_minute: 0,
            last_minute: 0,
            valid: false,
            close: tape_guest::Price::from_raw(0),
        }
    }

    fn push(&mut self, bar: &Bar) -> Option<tape_guest::Price> {
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        if minute_ns <= 0 {
            return None;
        }

        let nanos = bar.close_ts.as_nanos();
        if nanos.rem_euclid(minute_ns) != 0 {
            self.bucket = None;
            self.count = 0;
            self.valid = false;
            return None;
        }

        let minute = nanos.div_euclid(minute_ns);
        let bucket = minute.saturating_sub(1).div_euclid(MINUTES_PER_FOUR_HOURS);

        if self.bucket != Some(bucket) {
            self.bucket = Some(bucket);
            self.count = 1;
            self.first_minute = minute;
            self.last_minute = minute;
            self.valid = true;
        } else {
            if minute == self.last_minute.saturating_add(1) {
                self.count = self.count.saturating_add(1);
            } else {
                self.valid = false;
            }
            self.last_minute = minute;
        }
        self.close = bar.close;

        let is_boundary = minute.rem_euclid(MINUTES_PER_FOUR_HOURS) == 0;
        let expected_first = bucket
            .saturating_mul(MINUTES_PER_FOUR_HOURS)
            .saturating_add(1);
        if is_boundary
            && self.valid
            && self.count == MINUTES_PER_FOUR_HOURS as usize
            && self.first_minute == expected_first
        {
            Some(self.close)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy)]
struct MetricPoint {
    ts: i64,
    equity_raw: i64,
    price_raw: i64,
}

struct DualHorizonConfirmation {
    builder: FourHourBuilder,
    closes: VecDeque<i64>,
    points: Vec<MetricPoint>,
    first_point: Option<MetricPoint>,
    last_point: Option<MetricPoint>,
    filled_position_raw: i64,
    completed_round_trips: u64,
}

impl DualHorizonConfirmation {
    fn observe(&mut self, ctx: &Ctx, bar: &Bar) {
        let point = MetricPoint {
            ts: bar.close_ts.as_nanos(),
            equity_raw: ctx.equity().raw(),
            price_raw: bar.close.raw(),
        };
        if self.first_point.is_none() {
            self.first_point = Some(point);
            self.points.push(point);
        }
        self.last_point = Some(point);
    }

    fn on_four_hour_close(&mut self, ctx: &mut Ctx, close: tape_guest::Price) {
        self.closes.push_back(close.raw());
        if self.closes.len() > HISTORY_LEN {
            self.closes.pop_front();
        }

        if let Some(point) = self.last_point {
            self.points.push(point);
        }

        if self.closes.len() < HISTORY_LEN {
            return;
        }

        let current = self.closes[HISTORY_LEN - 1];
        let reference_7d = self.closes[HISTORY_LEN - 1 - SEVEN_DAY_BARS];
        let reference_30d = self.closes[HISTORY_LEN - 1 - THIRTY_DAY_BARS];
        let enter = current > reference_7d && current > reference_30d;
        let exit = current < reference_7d || current < reference_30d;

        if ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if enter && position.is_zero() {
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        } else if exit && position.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
        }
    }

    fn percent_change(from_raw: i64, to_raw: i64) -> Option<f64> {
        if from_raw <= 0 {
            None
        } else {
            Some((to_raw as f64 / from_raw as f64 - 1.0) * 100.0)
        }
    }

    fn percent_text(value: Option<f64>) -> String {
        match value {
            Some(value) => format!("{value:.2}%"),
            None => "n/a".to_string(),
        }
    }
}

impl Strategy for DualHorizonConfirmation {
    fn name(&self) -> String {
        "Dual-Horizon Confirmation".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        self.observe(ctx, bar);
        if let Some(close) = self.builder.push(bar) {
            self.on_four_hour_close(ctx, close);
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Buy {
            self.filled_position_raw = self
                .filled_position_raw
                .saturating_add(fill.qty.raw());
        } else if self.filled_position_raw > 0 {
            self.filled_position_raw = self
                .filled_position_raw
                .saturating_sub(fill.qty.raw());
            if self.filled_position_raw <= 0 {
                self.filled_position_raw = 0;
                self.completed_round_trips = self.completed_round_trips.saturating_add(1);
            }
        }
    }

    fn on_finish(&mut self, ctx: &mut Ctx) {
        let fallback = self.last_point;
        let final_point = MetricPoint {
            ts: ctx.now().as_nanos(),
            equity_raw: ctx.equity().raw(),
            price_raw: ctx
                .last_price(INSTRUMENT)
                .map(|price| price.raw())
                .or_else(|| fallback.map(|point| point.price_raw))
                .unwrap_or(0),
        };
        self.points.push(final_point);

        ctx.log(&format!(
            "Dual-Horizon Confirmation: completed round trips={}",
            self.completed_round_trips
        ));

        if let Some(first) = self.first_point {
            let end_ts = final_point.ts.max(first.ts);
            let midpoint_ts = first.ts.saturating_add(end_ts.saturating_sub(first.ts) / 2);
            let midpoint = self
                .points
                .iter()
                .copied()
                .min_by_key(|point| point.ts.abs_diff(midpoint_ts))
                .unwrap_or(first);

            let first_half = Self::percent_change(first.equity_raw, midpoint.equity_raw);
            let second_half = Self::percent_change(midpoint.equity_raw, final_point.equity_raw);
            let total = Self::percent_change(first.equity_raw, final_point.equity_raw);
            let buy_hold_first_half = Self::percent_change(first.price_raw, midpoint.price_raw);
            let buy_hold_second_half =
                Self::percent_change(midpoint.price_raw, final_point.price_raw);
            let buy_hold_total = Self::percent_change(first.price_raw, final_point.price_raw);

            ctx.log(&format!(
                "Performance: strategy first half={}, second half={}, total={}; gross buy-and-hold first half={}, second half={}, total={}",
                Self::percent_text(first_half),
                Self::percent_text(second_half),
                Self::percent_text(total),
                Self::percent_text(buy_hold_first_half),
                Self::percent_text(buy_hold_second_half),
                Self::percent_text(buy_hold_total),
            ));
        }
    }
}

export_strategy!(DualHorizonConfirmation {
    builder: FourHourBuilder::new(),
    closes: VecDeque::with_capacity(HISTORY_LEN + 1),
    points: Vec::new(),
    first_point: None,
    last_point: None,
    filled_position_raw: 0,
    completed_round_trips: 0,
});
