use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, TimeInForce,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const MINUTES_PER_HOUR: i64 = 60;
const BARS_PER_HOUR: u32 = 60;
const HOURS_FOR_IMPACT: usize = 6;
const HISTORY_HOURS: usize = 30 * 24;
const PARTICIPATION_NUM: i64 = 1;
const PARTICIPATION_DEN: i64 = 10;
const SIZING_MARGIN_PPM: i64 = 5_000;
const ENTRY_THRESHOLD: f64 = 1.25;
const EXIT_THRESHOLD: f64 = 2.5;
const MIN_ROUND_TRIPS: u32 = 24;

#[derive(Clone, Copy)]
struct HourStats {
    abs_log_return: f64,
    dollar_volume: f64,
}

struct LiquidityStressGate {
    last_close: Option<f64>,
    hour_return_sum: f64,
    hour_dollar_volume: f64,
    hour_bar_count: u32,
    recent_hours: VecDeque<HourStats>,
    historical_a: VecDeque<f64>,
    target_btc: bool,
    open_round_trip_qty_raw: i128,
    completed_round_trips: u32,
}

impl LiquidityStressGate {
    fn new() -> Self {
        Self {
            last_close: None,
            hour_return_sum: 0.0,
            hour_dollar_volume: 0.0,
            hour_bar_count: 0,
            recent_hours: VecDeque::with_capacity(HOURS_FOR_IMPACT + 1),
            historical_a: VecDeque::with_capacity(HISTORY_HOURS + 1),
            target_btc: false,
            open_round_trip_qty_raw: 0,
            completed_round_trips: 0,
        }
    }

    fn complete_hour(&mut self) -> Option<f64> {
        if self.hour_bar_count == BARS_PER_HOUR {
            self.recent_hours.push_back(HourStats {
                abs_log_return: self.hour_return_sum,
                dollar_volume: self.hour_dollar_volume,
            });
            if self.recent_hours.len() > HOURS_FOR_IMPACT {
                self.recent_hours.pop_front();
            }
        }

        self.hour_return_sum = 0.0;
        self.hour_dollar_volume = 0.0;
        self.hour_bar_count = 0;

        if self.recent_hours.len() != HOURS_FOR_IMPACT {
            return None;
        }

        let returns: f64 = self.recent_hours.iter().map(|h| h.abs_log_return).sum();
        let dollars: f64 = self.recent_hours.iter().map(|h| h.dollar_volume).sum();
        (dollars > 0.0 && dollars.is_finite()).then_some(returns / dollars)
    }

    fn historical_median(&self) -> Option<f64> {
        if self.historical_a.len() != HISTORY_HOURS {
            return None;
        }
        let mut values: Vec<f64> = self.historical_a.iter().copied().collect();
        values.sort_by(|a, b| a.total_cmp(b));
        let mid = values.len() / 2;
        Some((values[mid - 1] * 0.5) + (values[mid] * 0.5))
    }

    fn push_historical_a(&mut self, value: f64) {
        self.historical_a.push_back(value);
        if self.historical_a.len() > HISTORY_HOURS {
            self.historical_a.pop_front();
        }
    }

    fn submit_toward_target(&self, ctx: &mut Ctx, bar: &Bar) {
        if ctx.open_orders() != 0 || !bar.close.is_positive() {
            return;
        }

        let volume_cap = bar
            .volume
            .mul_ratio(PARTICIPATION_NUM, PARTICIPATION_DEN);
        if !volume_cap.is_positive() {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        let cap_raw = i128::from(volume_cap.raw());

        if !self.target_btc {
            if position.is_positive() {
                let qty_raw = i128::from(position.raw()).min(cap_raw);
                if qty_raw > 0 {
                    ctx.submit(
                        OrderRequest::market(
                            INSTRUMENT,
                            Side::Sell,
                            Qty::from_raw(qty_raw as i64),
                        )
                        .with_time_in_force(TimeInForce::Ioc),
                    );
                }
            }
            return;
        }

        let equity = ctx.equity();
        if !equity.is_positive() {
            return;
        }
        let target_qty = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            equity,
            SIZING_MARGIN_PPM,
        );
        let position_raw = i128::from(position.raw());
        let target_raw = i128::from(target_qty.raw());

        if target_raw > position_raw {
            let cash_affordable = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            let needed = target_raw - position_raw;
            let qty_raw = needed
                .min(i128::from(cash_affordable.raw()))
                .min(cap_raw);
            if qty_raw > 0 {
                ctx.submit(
                    OrderRequest::market(
                        INSTRUMENT,
                        Side::Buy,
                        Qty::from_raw(qty_raw as i64),
                    )
                    .with_time_in_force(TimeInForce::Ioc),
                );
            }
        } else if position_raw > target_raw {
            let qty_raw = (position_raw - target_raw).min(cap_raw);
            if qty_raw > 0 {
                ctx.submit(
                    OrderRequest::market(
                        INSTRUMENT,
                        Side::Sell,
                        Qty::from_raw(qty_raw as i64),
                    )
                    .with_time_in_force(TimeInForce::Ioc),
                );
            }
        }
    }
}

impl Strategy for LiquidityStressGate {
    fn name(&self) -> String {
        "liquidity-stress-gate".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let close = bar.close.to_f64();
        let volume = bar.volume.to_f64();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let minute_index = bar.close_ts.as_nanos().div_euclid(minute_ns);

        if let Some(previous) = self.last_close {
            if previous > 0.0 && close > 0.0 {
                self.hour_return_sum += (close / previous).ln().abs();
            }
        }
        self.last_close = Some(close);

        let dollar_volume = close * volume;
        if dollar_volume.is_finite() && dollar_volume > 0.0 {
            self.hour_dollar_volume += dollar_volume;
        }
        self.hour_bar_count = self.hour_bar_count.saturating_add(1);

        // The bar closing on the UTC hour boundary completes that hour.
        if minute_index.rem_euclid(MINUTES_PER_HOUR) == 0 {
            if let Some(current_a) = self.complete_hour() {
                // Compute the benchmark before adding the current observation.
                if let Some(baseline) = self.historical_median() {
                    if baseline > 0.0 {
                        let ratio = current_a / baseline;
                        if self.target_btc {
                            if ratio >= EXIT_THRESHOLD {
                                self.target_btc = false;
                            }
                        } else if ratio <= ENTRY_THRESHOLD {
                            self.target_btc = true;
                        }
                    } else if current_a > 0.0 && self.target_btc {
                        // A positive impact against a zero baseline is maximally stressed.
                        self.target_btc = false;
                    }
                }
                self.push_historical_a(current_a);
            }
        }

        // IOC orders limit each attempt to 10% of the latest completed bar's volume;
        // any remaining target is reconsidered on the next bar.
        self.submit_toward_target(ctx, bar);
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        match fill.side {
            Side::Buy => {
                self.open_round_trip_qty_raw += i128::from(fill.qty.raw());
            }
            Side::Sell => {
                let before = self.open_round_trip_qty_raw;
                self.open_round_trip_qty_raw -= i128::from(fill.qty.raw());
                if before > 0 && self.open_round_trip_qty_raw <= 0 {
                    self.completed_round_trips = self.completed_round_trips.saturating_add(1);
                    self.open_round_trip_qty_raw = 0;
                }
            }
        }
    }

    fn on_finish(&mut self, ctx: &mut Ctx) {
        if self.completed_round_trips < MIN_ROUND_TRIPS {
            ctx.log(&format!(
                "Insufficient activity to evaluate: {} completed round trips (fewer than {}).",
                self.completed_round_trips, MIN_ROUND_TRIPS
            ));
        }
    }
}

export_strategy!(LiquidityStressGate::new());
