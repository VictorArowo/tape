use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, Fill, InstrumentId, OrderRequest, Side, Strategy, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const WARMUP_BARS: usize = 1_440;
const EMA_LEN: usize = 360;
const R6_LAG: usize = 360;
const R3_LAG: usize = 180;
const COOLDOWN_BARS: u64 = 120;
const SIZING_MARGIN_PPM: i64 = 5_000;
const EMA_ALPHA: f64 = 2.0 / (EMA_LEN as f64 + 1.0);

struct DownsideCascadeBrake {
    closes: VecDeque<tape_guest::Price>,
    bars_seen: u64,
    ema: Option<f64>,
    ema_seed_sum: f64,
    ema_seed_count: usize,
    last_exit_bar: Option<u64>,
    exiting: bool,
    round_trip_bought: i128,
    round_trip_sold: i128,
    completed_round_trips: u64,
}

impl DownsideCascadeBrake {
    fn new() -> Self {
        Self {
            closes: VecDeque::with_capacity(WARMUP_BARS),
            bars_seen: 0,
            ema: None,
            ema_seed_sum: 0.0,
            ema_seed_count: 0,
            last_exit_bar: None,
            exiting: false,
            round_trip_bought: 0,
            round_trip_sold: 0,
            completed_round_trips: 0,
        }
    }

    fn record_close(&mut self, close: tape_guest::Price) {
        self.closes.push_back(close);
        if self.closes.len() > WARMUP_BARS {
            self.closes.pop_front();
        }

        if let Some(ema) = self.ema {
            self.ema = Some(ema + EMA_ALPHA * (close.to_f64() - ema));
        } else {
            self.ema_seed_sum += close.to_f64();
            self.ema_seed_count += 1;
            if self.ema_seed_count == EMA_LEN {
                self.ema = Some(self.ema_seed_sum / EMA_LEN as f64);
            }
        }
    }
}

impl Strategy for DownsideCascadeBrake {
    fn name(&self) -> String {
        "Downside Cascade Brake".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        self.bars_seen = self.bars_seen.saturating_add(1);
        self.record_close(bar.close);

        if self.closes.len() < WARMUP_BARS {
            return;
        }

        let close_360 = self.closes[self.closes.len() - 1 - R6_LAG];
        let close_180 = self.closes[self.closes.len() - 1 - R3_LAG];
        let h24_raw = self.closes.iter().map(|price| price.raw()).max().unwrap_or(0);
        let close_raw = i128::from(bar.close.raw());
        let close_360_raw = i128::from(close_360.raw());
        let close_180_raw = i128::from(close_180.raw());

        let r6_shock = close_360_raw > 0 && close_raw * 1_000 <= close_360_raw * 975;
        let below_h24_brake = close_raw * 100 <= i128::from(h24_raw) * 96;
        let r3_recovered = close_180_raw > 0 && close_raw * 100 >= close_180_raw * 101;
        let above_ema = self.ema.is_some_and(|ema| bar.close.to_f64() > ema);

        let position = ctx.position(INSTRUMENT);

        if self.exiting {
            if position.is_positive() {
                if ctx.open_orders() == 0 {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                }
                return;
            }
            self.exiting = false;
        }

        if position.is_positive() {
            if r6_shock || below_h24_brake {
                self.exiting = true;
                if ctx.open_orders() == 0 {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                }
            }
            return;
        }

        if !position.is_zero() {
            return;
        }

        let cooldown_complete = self.last_exit_bar.is_none_or(|exit_bar| {
            self.bars_seen.saturating_sub(exit_bar) >= COOLDOWN_BARS
        });
        if cooldown_complete && r3_recovered && above_ema && ctx.open_orders() == 0 {
            let qty = ctx.affordable_qty(INSTRUMENT, bar.close, ctx.cash(), SIZING_MARGIN_PPM);
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &Fill) {
        if fill.side == Side::Buy {
            if self.round_trip_bought == 0 {
                self.round_trip_sold = 0;
            }
            self.round_trip_bought += i128::from(fill.qty.raw());
        } else if fill.side == Side::Sell {
            self.last_exit_bar = Some(self.bars_seen);
            if self.round_trip_bought > 0 {
                self.round_trip_sold += i128::from(fill.qty.raw());
                if self.round_trip_sold >= self.round_trip_bought {
                    self.completed_round_trips = self.completed_round_trips.saturating_add(1);
                    self.round_trip_bought = 0;
                    self.round_trip_sold = 0;
                }
            }
        }
    }

    fn on_finish(&mut self, ctx: &mut Ctx) {
        if self.completed_round_trips < 30 {
            ctx.log(&format!(
                "Insufficient evidence: only {} completed round trips (30 required).",
                self.completed_round_trips
            ));
        } else {
            ctx.log(&format!(
                "Completed {} round trips.",
                self.completed_round_trips
            ));
        }
    }
}

export_strategy!(DownsideCascadeBrake::new());