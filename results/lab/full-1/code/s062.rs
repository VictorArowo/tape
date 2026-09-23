use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const WEEK_MINUTES: i64 = 7 * 24 * 60;
const FRIDAY_20_MINUTE_OF_WEEK: i64 = 1 * 24 * 60 + 20 * 60;
const MONDAY_00_MINUTE_OF_WEEK: i64 = 4 * 24 * 60;
const HISTORY_CLOSES: usize = 24 * 60 + 1;

struct WeekendLiquidityRiskGate {
    closes: VecDeque<(i64, i64)>,
    weekly_semivariance: VecDeque<(i64, f64)>,
    risk_off: bool,
}

impl WeekendLiquidityRiskGate {
    fn push_close(&mut self, timestamp_ns: i64, raw_close: i64) {
        self.closes.push_back((timestamp_ns, raw_close));
        let cutoff = timestamp_ns.saturating_sub(TimeDelta::from_mins(24 * 60).as_nanos());
        while self.closes.front().map_or(false, |entry| entry.0 < cutoff) {
            self.closes.pop_front();
        }
    }

    fn trailing_metrics(&self, timestamp_ns: i64) -> Option<(f64, f64)> {
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_ago = timestamp_ns.checked_sub(TimeDelta::from_mins(24 * 60).as_nanos())?;
        let mut old_close = None;
        for &(ts, raw) in &self.closes {
            if ts == day_ago {
                old_close = Some(raw);
                break;
            }
        }
        let old_close = old_close?;
        let (last_ts, last_close) = *self.closes.back()?;
        if last_ts != timestamp_ns || old_close <= 0 || last_close <= 0 {
            return None;
        }

        let return_24h = last_close as f64 / old_close as f64 - 1.0;

        // Six hours of one-minute returns require 361 closes, including both endpoints.
        const SIX_HOUR_CLOSES: usize = 6 * 60 + 1;
        if self.closes.len() < SIX_HOUR_CLOSES {
            return None;
        }
        let start = self.closes.len() - SIX_HOUR_CLOSES;
        let mut downside_semivariance = 0.0;
        for index in (start + 1)..self.closes.len() {
            let (previous_ts, previous_close) = self.closes.get(index - 1).copied()?;
            let (current_ts, current_close) = self.closes.get(index).copied()?;
            if current_ts - previous_ts != minute_ns || previous_close <= 0 || current_close <= 0 {
                return None;
            }
            let log_return = (current_close as f64 / previous_close as f64).ln();
            if log_return < 0.0 {
                downside_semivariance += log_return * log_return;
            }
        }
        Some((return_24h, downside_semivariance))
    }

    fn median_previous_eight_weeks(&self, week: i64) -> Option<f64> {
        let mut values = Vec::with_capacity(8);
        for offset in 1_i64..=8_i64 {
            let wanted_week = week - offset;
            let mut value = None;
            for &(recorded_week, semivariance) in &self.weekly_semivariance {
                if recorded_week == wanted_week {
                    value = Some(semivariance);
                    break;
                }
            }
            values.push(value?);
        }
        values.sort_by(|a, b| a.total_cmp(b));
        Some((values[3] + values[4]) / 2.0)
    }

    fn record_week(&mut self, week: i64, semivariance: f64) {
        self.weekly_semivariance.retain(|entry| entry.0 != week);
        self.weekly_semivariance.push_back((week, semivariance));
        let oldest_week = week - 8;
        while self
            .weekly_semivariance
            .front()
            .map_or(false, |entry| entry.0 < oldest_week)
        {
            self.weekly_semivariance.pop_front();
        }
    }
}

impl Strategy for WeekendLiquidityRiskGate {
    fn name(&self) -> String {
        "Weekend Liquidity Risk Gate".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let timestamp_ns = bar.close_ts.as_nanos();
        self.push_close(timestamp_ns, bar.close.raw());

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let minute_index = timestamp_ns.div_euclid(minute_ns);
        let minute_of_week = minute_index.rem_euclid(WEEK_MINUTES);
        let week = minute_index.div_euclid(WEEK_MINUTES);

        if minute_of_week == FRIDAY_20_MINUTE_OF_WEEK && !self.risk_off {
            if let Some((return_24h, semivariance)) = self.trailing_metrics(timestamp_ns) {
                let median = self.median_previous_eight_weeks(week);
                self.record_week(week, semivariance);
                if return_24h <= 0.0025 && median.map_or(false, |value| semivariance > value) {
                    self.risk_off = true;
                }
            }
        }

        if !self.risk_off {
            return;
        }

        if minute_of_week == MONDAY_00_MINUTE_OF_WEEK {
            // Submit at the Monday 00:00 close so the sandbox can execute at the next open.
            if ctx.open_orders() == 0 {
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
            self.risk_off = false;
        } else if ctx.position(INSTRUMENT).is_positive() && ctx.open_orders() == 0 {
            // Retry any unfilled remainder while the strategy is in its risk-off period.
            ctx.submit(OrderRequest::market(
                INSTRUMENT,
                Side::Sell,
                ctx.position(INSTRUMENT),
            ));
        }
    }
}

export_strategy!(WeekendLiquidityRiskGate {
    closes: VecDeque::with_capacity(HISTORY_CLOSES),
    weekly_semivariance: VecDeque::with_capacity(8),
    risk_off: false,
});
