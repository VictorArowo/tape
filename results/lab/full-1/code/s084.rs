use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const BASELINE_HOURS: i64 = 720;
const BUY_THRESHOLD: f64 = 1.25;
const SELL_THRESHOLD: f64 = 2.0;
const SIZING_MARGIN_PPM: i64 = 5_000;
const MAX_IMPACT_HISTORY: usize = 800;

struct HourBuilder {
    key: i64,
    close: tape_guest::Price,
    turnover: f64,
}

struct HourSample {
    key: i64,
    close: tape_guest::Price,
    turnover: f64,
}

struct PriceImpactLiquidityGate {
    current_hour: Option<HourBuilder>,
    hours: VecDeque<HourSample>,
    impacts: VecDeque<(i64, f64)>,
    // None until a signal establishes a target; Some(true) means long,
    // Some(false) means flat. Unfilled IOC targets are retried on later bars.
    target_long: Option<bool>,
}

impl PriceImpactLiquidityGate {
    fn new() -> Self {
        Self {
            current_hour: None,
            hours: VecDeque::with_capacity(7),
            impacts: VecDeque::with_capacity(MAX_IMPACT_HISTORY),
            target_long: None,
        }
    }

    fn complete_hour(&mut self, ctx: &mut Ctx) {
        let Some(completed) = self.current_hour.take() else {
            return;
        };

        self.hours.push_back(HourSample {
            key: completed.key,
            close: completed.close,
            turnover: completed.turnover,
        });
        while self.hours.len() > 7 {
            self.hours.pop_front();
        }

        let Some((key, impact)) = self.latest_impact() else {
            return;
        };
        self.impacts.push_back((key, impact));
        while self.impacts.len() > MAX_IMPACT_HISTORY {
            self.impacts.pop_front();
        }

        let Some(baseline) = self.baseline_for(key) else {
            return;
        };

        let position = ctx.position(INSTRUMENT);
        if position.is_zero() && impact < BUY_THRESHOLD * baseline {
            self.target_long = Some(true);
        } else if position.is_positive() && impact > SELL_THRESHOLD * baseline {
            self.target_long = Some(false);
        }
    }

    fn latest_impact(&self) -> Option<(i64, f64)> {
        if self.hours.len() != 7 {
            return None;
        }

        let mut previous_key = None;
        for hour in &self.hours {
            if let Some(previous) = previous_key {
                if hour.key != previous + 1 {
                    return None;
                }
            }
            previous_key = Some(hour.key);
        }

        let first = self.hours.front()?;
        let last = self.hours.back()?;
        let first_close = first.close.to_f64();
        if first_close <= 0.0 {
            return None;
        }

        // The seven closes span six hours; turnover is summed over the six
        // completed hourly buckets after the reference close.
        let turnover: f64 = self.hours.iter().skip(1).map(|hour| hour.turnover).sum();
        if turnover <= 0.0 {
            return None;
        }

        let return_abs = (last.close.to_f64() / first_close - 1.0).abs();
        let impact = return_abs / turnover;
        impact.is_finite().then_some((last.key, impact))
    }

    fn baseline_for(&self, current_key: i64) -> Option<f64> {
        // Exclude prior impacts whose six-hour windows overlap the current
        // six-hour window. The 720-hour baseline ends at current_key - 6.
        let upper = current_key.checked_sub(6)?;
        let lower = upper.checked_sub(BASELINE_HOURS - 1)?;
        let mut values = Vec::with_capacity(BASELINE_HOURS as usize);

        for &(key, value) in &self.impacts {
            if (lower..=upper).contains(&key) {
                values.push(value);
            }
        }
        if values.len() != BASELINE_HOURS as usize {
            return None;
        }

        values.sort_by(f64::total_cmp);
        let middle = values.len() / 2;
        if values.len() % 2 == 0 {
            Some((values[middle - 1] + values[middle]) / 2.0)
        } else {
            Some(values[middle])
        }
    }

    fn execute_target(&self, ctx: &mut Ctx, bar: &Bar) {
        let Some(target_long) = self.target_long else {
            return;
        };
        if ctx.open_orders() != 0 {
            return;
        }

        // Only already-observed volume is available when submitting an order.
        // The IOC remainder is retried on a later bar against that bar's volume.
        let volume_cap: Qty = bar.volume.mul_ratio(1, 10);
        if !volume_cap.is_positive() {
            return;
        }

        if target_long {
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            let qty = if affordable < volume_cap {
                affordable
            } else {
                volume_cap
            };
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        } else {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                let qty = if position < volume_cap {
                    position
                } else {
                    volume_cap
                };
                if qty.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
                }
            }
        }
    }
}

impl Strategy for PriceImpactLiquidityGate {
    fn name(&self) -> String {
        "price-impact-liquidity-gate".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let hour_ns = TimeDelta::from_hours(1).as_nanos();
        let key = bar.open_ts.as_nanos() / hour_ns;

        if self
            .current_hour
            .as_ref()
            .is_some_and(|current| current.key != key)
        {
            self.complete_hour(ctx);
        }

        if self.current_hour.is_none() {
            self.current_hour = Some(HourBuilder {
                key,
                close: bar.close,
                turnover: 0.0,
            });
        }

        if let Some(current) = self.current_hour.as_mut() {
            current.close = bar.close;
            current.turnover += (bar.close * bar.volume).to_f64();
        }

        let close_ns = bar.close_ts.as_nanos();
        if close_ns.rem_euclid(hour_ns) == 0 && close_ns / hour_ns > key {
            self.complete_hour(ctx);
        }

        self.execute_target(ctx, bar);
    }
}

export_strategy!(PriceImpactLiquidityGate::new());
