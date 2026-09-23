use std::collections::VecDeque;

use tape_guest::{export_strategy, Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const VOL_RETURN_WINDOW: usize = 7;
const VOL_RANK_WINDOW: usize = 90;
const BUY_PERCENTILE: f64 = 0.60;
const SELL_PERCENTILE: f64 = 0.85;
const SIZING_MARGIN_PPM: i64 = 5_000;
const SLIPPAGE_PPM: i64 = 200;

#[derive(Clone, Copy)]
enum Intent {
    Buy,
    Sell,
}

struct HystereticVolatilityBrake {
    current_day: Option<i64>,
    current_day_close: Option<Price>,
    last_finalized_day: Option<i64>,
    last_daily_close: Option<Price>,
    daily_returns: VecDeque<f64>,
    volatility_readings: VecDeque<f64>,
    target_long: Option<bool>,
    intent: Option<Intent>,
    remaining: Qty,
}

impl HystereticVolatilityBrake {
    fn new() -> Self {
        Self {
            current_day: None,
            current_day_close: None,
            last_finalized_day: None,
            last_daily_close: None,
            daily_returns: VecDeque::with_capacity(VOL_RETURN_WINDOW),
            volatility_readings: VecDeque::with_capacity(VOL_RANK_WINDOW),
            target_long: None,
            intent: None,
            remaining: Qty::from_raw(0),
        }
    }

    fn set_intent(&mut self, intent: Intent, qty: Qty) {
        if qty.is_positive() {
            self.intent = Some(intent);
            self.remaining = qty;
        } else {
            self.intent = None;
            self.remaining = Qty::from_raw(0);
        }
    }

    fn finalize_day(&mut self, ctx: &mut Ctx, day: i64, close: Price) {
        if self.last_finalized_day == Some(day) {
            return;
        }
        self.last_finalized_day = Some(day);

        let close_f64 = close.to_f64();
        if close_f64 <= 0.0 {
            return;
        }

        if let Some(previous_close) = self.last_daily_close {
            let previous_f64 = previous_close.to_f64();
            if previous_f64 > 0.0 {
                let daily_return = (close_f64 / previous_f64).ln();
                if self.daily_returns.len() == VOL_RETURN_WINDOW {
                    self.daily_returns.pop_front();
                }
                self.daily_returns.push_back(daily_return);
            }
        }
        self.last_daily_close = Some(close);

        if self.daily_returns.len() != VOL_RETURN_WINDOW {
            return;
        }

        let mean = self.daily_returns.iter().sum::<f64>() / VOL_RETURN_WINDOW as f64;
        let variance = self
            .daily_returns
            .iter()
            .map(|value| {
                let difference = value - mean;
                difference * difference
            })
            .sum::<f64>()
            / VOL_RETURN_WINDOW as f64;
        let reading = variance.sqrt();

        if self.volatility_readings.len() == VOL_RANK_WINDOW {
            let less = self
                .volatility_readings
                .iter()
                .filter(|&&value| value < reading)
                .count();
            let equal = self
                .volatility_readings
                .iter()
                .filter(|&&value| value == reading)
                .count();
            let percentile = (less as f64 + 0.5 * equal as f64) / VOL_RANK_WINDOW as f64;

            let was_target_long = self
                .target_long
                .unwrap_or_else(|| ctx.position(INSTRUMENT).is_positive());
            self.target_long = Some(was_target_long);

            if was_target_long && percentile >= SELL_PERCENTILE {
                self.target_long = Some(false);
                self.set_intent(Intent::Sell, ctx.position(INSTRUMENT));
            } else if !was_target_long && percentile <= BUY_PERCENTILE {
                self.target_long = Some(true);
                let estimated_fill_price = close.mul_ratio(1_000_000 + SLIPPAGE_PPM, 1_000_000);
                let qty = ctx.affordable_qty(
                    INSTRUMENT,
                    estimated_fill_price,
                    ctx.cash(),
                    SIZING_MARGIN_PPM,
                );
                self.set_intent(Intent::Buy, qty);
            }
        }

        if self.volatility_readings.len() == VOL_RANK_WINDOW {
            self.volatility_readings.pop_front();
        }
        self.volatility_readings.push_back(reading);
    }

    fn submit_remainder(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if self.intent.is_none() || !self.remaining.is_positive() || ctx.open_orders() != 0 {
            return;
        }

        // Use the latest completed bar's volume as the causal participation cap.
        let volume_cap = bar.volume.mul_ratio(1, 10);
        if !volume_cap.is_positive() {
            return;
        }

        let (side, qty) = match self.intent {
            Some(Intent::Buy) => {
                let estimated_fill_price =
                    bar.close.mul_ratio(1_000_000 + SLIPPAGE_PPM, 1_000_000);
                let affordable = ctx.affordable_qty(
                    INSTRUMENT,
                    estimated_fill_price,
                    ctx.cash(),
                    SIZING_MARGIN_PPM,
                );
                let qty = min_qty(min_qty(self.remaining, volume_cap), affordable);
                (Side::Buy, qty)
            }
            Some(Intent::Sell) => {
                let position = ctx.position(INSTRUMENT);
                if !position.is_positive() {
                    self.intent = None;
                    self.remaining = Qty::from_raw(0);
                    return;
                }
                let qty = min_qty(min_qty(self.remaining, volume_cap), position);
                (Side::Sell, qty)
            }
            None => return,
        };

        if qty.is_positive() {
            let _order_id = ctx.submit(OrderRequest::market(INSTRUMENT, side, qty));
        }
    }
}

fn min_qty(left: Qty, right: Qty) -> Qty {
    Qty::from_raw(left.raw().min(right.raw()))
}

impl Strategy for HystereticVolatilityBrake {
    fn name(&self) -> String {
        "hysteretic-volatility-brake".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let day_ns = TimeDelta::from_days(1).as_nanos();
        let open_day = bar.open_ts.as_nanos().div_euclid(day_ns);
        let close_day = bar.close_ts.as_nanos().div_euclid(day_ns);

        if let Some(previous_day) = self.current_day {
            if open_day != previous_day {
                if let Some(previous_close) = self.current_day_close {
                    self.finalize_day(ctx, previous_day, previous_close);
                }
                self.current_day = Some(open_day);
                self.current_day_close = Some(bar.close);
            } else {
                self.current_day_close = Some(bar.close);
            }
        } else {
            self.current_day = Some(open_day);
            self.current_day_close = Some(bar.close);
        }

        // A minute bar ending at UTC midnight completes the day at its close.
        if close_day > open_day {
            if let Some(day_close) = self.current_day_close {
                self.finalize_day(ctx, open_day, day_close);
            }
        }

        self.submit_remainder(ctx, bar);
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        let matches_intent = matches!(
            (self.intent, fill.side),
            (Some(Intent::Buy), Side::Buy) | (Some(Intent::Sell), Side::Sell)
        );
        if !matches_intent {
            return;
        }

        if fill.qty >= self.remaining {
            self.remaining = Qty::from_raw(0);
            self.intent = None;
        } else {
            self.remaining = self.remaining - fill.qty;
        }
    }
}

export_strategy!(HystereticVolatilityBrake::new());
