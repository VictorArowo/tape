use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const MIN_HISTORY_RETURNS: usize = 7 * 24 * 4;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct ReturnPoint {
    ts_ns: i64,
    value: f64,
}

struct DownsideJumpCooldown {
    last_15m_close: Option<(i64, i64)>,
    returns: VecDeque<ReturnPoint>,
    minute_closes: VecDeque<(i64, i64)>,
    cooldown_until_ns: Option<i64>,
    exit_required: bool,
}

impl DownsideJumpCooldown {
    fn prior_stddev(&self, now_ns: i64, seven_days_ns: i64) -> Option<f64> {
        if self.returns.len() < MIN_HISTORY_RETURNS {
            return None;
        }
        let oldest = self.returns.front()?.ts_ns;
        if oldest > now_ns.saturating_sub(seven_days_ns) {
            return None;
        }

        // Population standard deviation of the completed returns in the lookback.
        let mut count = 0.0_f64;
        let mut mean = 0.0_f64;
        let mut sum_squared_deviations = 0.0_f64;
        for point in &self.returns {
            count += 1.0;
            let delta = point.value - mean;
            mean += delta / count;
            let delta_after_update = point.value - mean;
            sum_squared_deviations += delta * delta_after_update;
        }
        (count > 0.0).then(|| (sum_squared_deviations / count).sqrt())
    }
}

impl Strategy for DownsideJumpCooldown {
    fn name(&self) -> String {
        "downside-jump-cooldown".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let now_ns = bar.close_ts.as_nanos();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let fifteen_minutes_ns = TimeDelta::from_mins(15).as_nanos();
        let sixty_minutes_ns = TimeDelta::from_mins(60).as_nanos();
        let seven_days_ns = TimeDelta::from_days(7).as_nanos();
        let thirty_days_ns = TimeDelta::from_days(30).as_nanos();
        let six_hours_ns = TimeDelta::from_hours(6).as_nanos();

        let mut jump_signal = false;
        if now_ns.rem_euclid(fifteen_minutes_ns) == 0 {
            let cutoff_ns = now_ns.saturating_sub(thirty_days_ns);
            while self
                .returns
                .front()
                .is_some_and(|point| point.ts_ns < cutoff_ns)
            {
                self.returns.pop_front();
            }

            if let Some((previous_ts_ns, previous_close_raw)) = self.last_15m_close {
                if now_ns.saturating_sub(previous_ts_ns) == fifteen_minutes_ns
                    && previous_close_raw > 0
                    && bar.close.raw() > 0
                {
                    let return_value = bar.close.raw() as f64 / previous_close_raw as f64 - 1.0;
                    if let Some(stddev) = self.prior_stddev(now_ns, seven_days_ns) {
                        jump_signal = return_value <= -3.0 * stddev && return_value <= -0.015;
                    }
                    // Add the current return only after evaluating it against prior returns.
                    self.returns.push_back(ReturnPoint {
                        ts_ns: now_ns,
                        value: return_value,
                    });
                }
            }
            self.last_15m_close = Some((now_ns, bar.close.raw()));
        }

        let sixty_minute_cutoff_ns = now_ns.saturating_sub(sixty_minutes_ns);
        while self
            .minute_closes
            .front()
            .is_some_and(|(ts_ns, _)| *ts_ns < sixty_minute_cutoff_ns)
        {
            self.minute_closes.pop_front();
        }
        let breakout = self.minute_closes.len() >= 60
            && self
                .minute_closes
                .iter()
                .map(|(_, close_raw)| *close_raw)
                .max()
                .is_some_and(|highest| bar.close.raw() > highest);
        self.minute_closes
            .push_back((now_ns, bar.close.raw()));

        let position = ctx.position(INSTRUMENT);
        if jump_signal && position.is_positive() {
            self.exit_required = true;
            let new_cooldown = now_ns.saturating_add(six_hours_ns);
            self.cooldown_until_ns = Some(
                self.cooldown_until_ns
                    .map_or(new_cooldown, |existing| existing.max(new_cooldown)),
            );
        }

        if self.exit_required && position.is_positive() {
            if ctx.open_orders() == 0 {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
            return;
        }

        if !position.is_positive() {
            self.exit_required = false;
            let cooldown_over = self.cooldown_until_ns.is_none_or(|until| now_ns >= until);
            if ctx.open_orders() == 0 && cooldown_over && breakout {
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
        }
    }
}

export_strategy!(DownsideJumpCooldown {
    last_15m_close: None,
    returns: VecDeque::new(),
    minute_closes: VecDeque::new(),
    cooldown_until_ns: None,
    exit_required: false,
});