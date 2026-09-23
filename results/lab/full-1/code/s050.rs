use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const HISTORY_LEN: usize = 30;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct HourBucket {
    key: i64,
    close_raw: i64,
    volume_raw: i128,
}

struct UtcMarkPressureFade {
    active_hour: Option<HourBucket>,
    previous_hour_close_raw: Option<i64>,
    returns: VecDeque<f64>,
    volumes: VecDeque<i128>,
    holding_until_ns: Option<i64>,
}

impl UtcMarkPressureFade {
    fn push_history<T>(window: &mut VecDeque<T>, value: T) {
        window.push_back(value);
        if window.len() > HISTORY_LEN {
            window.pop_front();
        }
    }

    fn finish_hour(&mut self, ctx: &mut Ctx, hour: HourBucket, midnight: bool, bar: &Bar) {
        let current_return = self.previous_hour_close_raw.and_then(|previous| {
            (previous > 0 && hour.close_raw > 0)
                .then(|| hour.close_raw as f64 / previous as f64 - 1.0)
        });

        let is_23_utc = hour.key.rem_euclid(TimeDelta::from_days(1).as_nanos())
            == 23 * TimeDelta::from_hours(1).as_nanos();

        if midnight && is_23_utc && self.returns.len() == HISTORY_LEN
            && self.volumes.len() == HISTORY_LEN
        {
            if let Some(hour_return) = current_return {
                let mean = self.returns.iter().sum::<f64>() / HISTORY_LEN as f64;
                let variance = self
                    .returns
                    .iter()
                    .map(|value| {
                        let difference = value - mean;
                        difference * difference
                    })
                    .sum::<f64>()
                    / HISTORY_LEN as f64;
                let stddev = variance.max(0.0).sqrt();

                let mut prior_volumes: Vec<f64> =
                    self.volumes.iter().map(|value| *value as f64).collect();
                prior_volumes.sort_by(f64::total_cmp);
                let median_volume =
                    (prior_volumes[HISTORY_LEN / 2 - 1] + prior_volumes[HISTORY_LEN / 2]) / 2.0;

                let volume_condition = hour.volume_raw as f64 > 1.5 * median_volume;
                let return_condition = hour_return < -1.5 * stddev;
                let position = ctx.position(INSTRUMENT);

                if return_condition
                    && volume_condition
                    && position.is_zero()
                    && self.holding_until_ns.is_none()
                    && ctx.open_orders() == 0
                {
                    let affordable = ctx.affordable_qty(
                        INSTRUMENT,
                        bar.close,
                        ctx.cash(),
                        SIZING_MARGIN_PPM,
                    );
                    let volume_cap = bar.volume.mul_ratio(1, 10);
                    let qty = if affordable < volume_cap {
                        affordable
                    } else {
                        volume_cap
                    };
                    if qty.is_positive() {
                        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                    }
                }
            }
        }

        if let Some(hour_return) = current_return {
            Self::push_history(&mut self.returns, hour_return);
        }
        Self::push_history(&mut self.volumes, hour.volume_raw);
        self.previous_hour_close_raw = Some(hour.close_raw);
    }
}

impl Strategy for UtcMarkPressureFade {
    fn name(&self) -> String {
        "utc-mark-pressure-fade".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_zero() {
            self.holding_until_ns = None;
        } else if let Some(exit_at) = self.holding_until_ns {
            if bar.close_ts.as_nanos() >= exit_at && ctx.open_orders() == 0 {
                let volume_cap = bar.volume.mul_ratio(1, 10);
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

        let hour_ns = TimeDelta::from_hours(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let hour_key = bar.open_ts.as_nanos().div_euclid(hour_ns);
        let midnight = bar.close_ts.as_nanos().rem_euclid(day_ns) == 0;
        let hour_boundary = bar.close_ts.as_nanos().rem_euclid(hour_ns) == 0;

        if self.active_hour.as_ref().is_some_and(|hour| hour.key != hour_key) {
            if let Some(hour) = self.active_hour.take() {
                self.finish_hour(ctx, hour, false, bar);
            }
        }

        let hour = self.active_hour.get_or_insert(HourBucket {
            key: hour_key,
            close_raw: bar.close.raw(),
            volume_raw: 0,
        });
        hour.close_raw = bar.close.raw();
        hour.volume_raw += i128::from(bar.volume.raw());

        if hour_boundary {
            if let Some(hour) = self.active_hour.take() {
                self.finish_hour(ctx, hour, midnight, bar);
            }
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        match fill.side {
            Side::Buy => {
                if self.holding_until_ns.is_none() {
                    self.holding_until_ns = fill
                        .ts
                        .as_nanos()
                        .checked_add(TimeDelta::from_hours(4).as_nanos());
                }
            }
            Side::Sell => {
                if ctx.position(INSTRUMENT).is_zero() {
                    self.holding_until_ns = None;
                }
            }
        }
    }
}

export_strategy!(UtcMarkPressureFade {
    active_hour: None,
    previous_hour_close_raw: None,
    returns: VecDeque::with_capacity(HISTORY_LEN + 1),
    volumes: VecDeque::with_capacity(HISTORY_LEN + 1),
    holding_until_ns: None,
});