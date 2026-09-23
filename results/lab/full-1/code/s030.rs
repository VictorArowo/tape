use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, TimeInForce,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const VWAP_HOURS: usize = 72;
const REQUIRED_STREAK: usize = 3;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct Hour {
    close_raw: i64,
    volume_raw: i128,
}

struct HourBuilder {
    key: i64,
    close_raw: i64,
    volume_raw: i128,
}

impl HourBuilder {
    fn finish(self) -> Hour {
        Hour {
            close_raw: self.close_raw,
            volume_raw: self.volume_raw,
        }
    }
}

struct RollingVwapRegime {
    history: VecDeque<Hour>,
    active_hour: Option<HourBuilder>,
    up_streak: usize,
    down_streak: usize,
}

impl RollingVwapRegime {
    fn finish_hour(&mut self, ctx: &mut Ctx, hour: Hour) {
        // Use only the preceding 72 completed hours; the hour just finished
        // is added to the rolling window after evaluating its close.
        let signal = if self.history.len() == VWAP_HOURS {
            let (weighted_sum, volume_sum) = self.history.iter().fold(
                (0.0_f64, 0.0_f64),
                |(weighted, volume), prior| {
                    (
                        weighted + prior.close_raw as f64 * prior.volume_raw as f64,
                        volume + prior.volume_raw as f64,
                    )
                },
            );

            if volume_sum > 0.0 {
                let vwap_raw = weighted_sum / volume_sum;
                let close_raw = hour.close_raw as f64;
                Some((close_raw >= vwap_raw * 1.01, close_raw <= vwap_raw * 0.99))
            } else {
                None
            }
        } else {
            None
        };

        match signal {
            Some((true, _)) => {
                self.up_streak = self.up_streak.saturating_add(1);
                self.down_streak = 0;
            }
            Some((_, true)) => {
                self.down_streak = self.down_streak.saturating_add(1);
                self.up_streak = 0;
            }
            _ => {
                self.up_streak = 0;
                self.down_streak = 0;
            }
        }

        if ctx.open_orders() == 0 {
            let position = ctx.position(INSTRUMENT);
            if position.is_zero() && self.up_streak >= REQUIRED_STREAK {
                let price = tape_guest::Price::from_raw(hour.close_raw);
                let qty = ctx.affordable_qty(
                    INSTRUMENT,
                    price,
                    ctx.cash(),
                    SIZING_MARGIN_PPM,
                );
                if qty.is_positive() {
                    ctx.submit(
                        OrderRequest::market(INSTRUMENT, Side::Buy, qty)
                            .with_time_in_force(TimeInForce::Ioc),
                    );
                }
            } else if position.is_positive() && self.down_streak >= REQUIRED_STREAK {
                ctx.submit(
                    OrderRequest::market(INSTRUMENT, Side::Sell, position)
                        .with_time_in_force(TimeInForce::Ioc),
                );
            }
        }

        self.history.push_back(hour);
        if self.history.len() > VWAP_HOURS {
            self.history.pop_front();
        }
    }
}

impl Strategy for RollingVwapRegime {
    fn name(&self) -> String {
        "rolling-vwap-regime".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let hour_ns = TimeDelta::from_hours(1).as_nanos();
        let open_hour = bar.open_ts.as_nanos().div_euclid(hour_ns);
        let close_hour = bar.close_ts.as_nanos().div_euclid(hour_ns);

        let changed_hour = match &self.active_hour {
            Some(active) => active.key != open_hour,
            None => false,
        };
        if changed_hour {
            if let Some(active) = self.active_hour.take() {
                self.finish_hour(ctx, active.finish());
            }
        }

        if self.active_hour.is_none() {
            self.active_hour = Some(HourBuilder {
                key: open_hour,
                close_raw: bar.close.raw(),
                volume_raw: 0,
            });
        }

        if let Some(active) = &mut self.active_hour {
            active.close_raw = bar.close.raw();
            active.volume_raw += i128::from(bar.volume.raw());
        }

        // A one-minute bar whose close crosses the hour boundary completes
        // the hourly aggregate, so an order can be submitted at that close.
        if close_hour != open_hour {
            if let Some(active) = self.active_hour.take() {
                self.finish_hour(ctx, active.finish());
            }
        }
    }
}

export_strategy!(RollingVwapRegime {
    history: VecDeque::with_capacity(VWAP_HOURS),
    active_hour: None,
    up_streak: 0,
    down_streak: 0,
});
