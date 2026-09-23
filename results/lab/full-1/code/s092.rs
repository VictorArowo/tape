use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const HISTORY_LEN: usize = 336;
const HISTORY_CAPACITY: usize = 400;
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy)]
struct MinuteSample {
    minute: i64,
    close_raw: i64,
    low_raw: i64,
    volume_raw: i64,
}

struct LiveBucket {
    id: i64,
    first_minute: i64,
    last_minute: i64,
    count: u8,
    contiguous: bool,
    volume_raw: i128,
    close_raw: i64,
}

struct BucketStat {
    end_minute: i64,
    volume_raw: i128,
    log_return: Option<f64>,
}

#[derive(Clone, Copy)]
struct Watch {
    recovery_level: Price,
    expires_ns: i64,
}

struct LiquidationFlushRebound {
    minute_window: VecDeque<MinuteSample>,
    live_bucket: Option<LiveBucket>,
    previous_complete: Option<(i64, i64)>,
    bucket_history: VecDeque<BucketStat>,
    watch: Option<Watch>,
    entry_fill_ns: Option<i64>,
    last_entry_ns: Option<i64>,
    pending_order: bool,
}

impl LiquidationFlushRebound {
    fn new() -> Self {
        Self {
            minute_window: VecDeque::with_capacity(31),
            live_bucket: None,
            previous_complete: None,
            bucket_history: VecDeque::with_capacity(HISTORY_CAPACITY),
            watch: None,
            entry_fill_ns: None,
            last_entry_ns: None,
            pending_order: false,
        }
    }

    fn push_minute(&mut self, minute: i64, bar: &Bar) {
        if let Some(last) = self.minute_window.back() {
            if last.minute.saturating_add(1) != minute {
                self.minute_window.clear();
            }
        }
        self.minute_window.push_back(MinuteSample {
            minute,
            close_raw: bar.close.raw(),
            low_raw: bar.low.raw(),
            volume_raw: bar.volume.raw(),
        });
        if self.minute_window.len() > 31 {
            self.minute_window.pop_front();
        }
    }

    fn finalize_bucket(&mut self, bucket: LiveBucket) {
        let expected_first = bucket.id.saturating_mul(30);
        let expected_last = expected_first.saturating_add(29);
        let complete = bucket.count == 30
            && bucket.contiguous
            && bucket.first_minute == expected_first
            && bucket.last_minute == expected_last;

        if !complete {
            self.previous_complete = None;
            return;
        }

        let log_return = self.previous_complete.and_then(|(previous_id, previous_close)| {
            if previous_id.saturating_add(1) != bucket.id
                || previous_close <= 0
                || bucket.close_raw <= 0
            {
                None
            } else {
                Some((bucket.close_raw as f64 / previous_close as f64).ln())
            }
        });

        self.bucket_history.push_back(BucketStat {
            end_minute: expected_last,
            volume_raw: bucket.volume_raw,
            log_return,
        });
        while self.bucket_history.len() > HISTORY_CAPACITY {
            self.bucket_history.pop_front();
        }
        self.previous_complete = Some((bucket.id, bucket.close_raw));
    }

    fn push_bucket_bar(&mut self, minute: i64, bar: &Bar) {
        let id = minute.div_euclid(30);
        let mut bucket = match self.live_bucket.take() {
            Some(mut current) if current.id == id => {
                if current.last_minute.saturating_add(1) != minute {
                    current.contiguous = false;
                }
                current.last_minute = minute;
                current.count = current.count.saturating_add(1);
                current.volume_raw += i128::from(bar.volume.raw());
                current.close_raw = bar.close.raw();
                current
            }
            Some(current) => {
                self.finalize_bucket(current);
                LiveBucket {
                    id,
                    first_minute: minute,
                    last_minute: minute,
                    count: 1,
                    contiguous: true,
                    volume_raw: i128::from(bar.volume.raw()),
                    close_raw: bar.close.raw(),
                }
            }
            None => LiveBucket {
                id,
                first_minute: minute,
                last_minute: minute,
                count: 1,
                contiguous: true,
                volume_raw: i128::from(bar.volume.raw()),
                close_raw: bar.close.raw(),
            },
        };
        // Keep the local mutable binding explicit so bucket updates remain straightforward.
        bucket.close_raw = bar.close.raw();
        self.live_bucket = Some(bucket);
    }

    fn shock_recovery_level(&self, minute: i64) -> Option<Price> {
        if self.minute_window.len() != 31 {
            return None;
        }

        let p0_raw = self.minute_window.front()?.close_raw;
        let close_raw = self.minute_window.back()?.close_raw;
        if p0_raw <= 0 || close_raw <= 0 {
            return None;
        }

        let mut low_raw = i64::MAX;
        let mut current_volume_raw = 0_i128;
        for sample in self.minute_window.iter().skip(1) {
            low_raw = low_raw.min(sample.low_raw);
            current_volume_raw += i128::from(sample.volume_raw);
        }
        if low_raw <= 0 || p0_raw <= low_raw {
            return None;
        }

        let cutoff_minute = minute.saturating_sub(30);
        let week_minutes = 7 * 24 * 60;
        let eligible: Vec<&BucketStat> = self
            .bucket_history
            .iter()
            .filter(|stat| {
                stat.end_minute <= cutoff_minute
                    && cutoff_minute.saturating_sub(stat.end_minute) <= week_minutes
            })
            .collect();
        if eligible.len() < HISTORY_LEN {
            return None;
        }

        let mut volumes: Vec<f64> = eligible
            .iter()
            .rev()
            .take(HISTORY_LEN)
            .map(|stat| stat.volume_raw as f64)
            .collect();
        volumes.sort_by(f64::total_cmp);
        let median_volume_raw = (volumes[HISTORY_LEN / 2 - 1] + volumes[HISTORY_LEN / 2]) / 2.0;
        if median_volume_raw <= 0.0 || (current_volume_raw as f64) < 2.0 * median_volume_raw {
            return None;
        }

        let mut returns: Vec<f64> = eligible
            .iter()
            .rev()
            .filter_map(|stat| stat.log_return)
            .take(HISTORY_LEN)
            .collect();
        if returns.len() < HISTORY_LEN {
            return None;
        }
        let mean = returns.iter().sum::<f64>() / HISTORY_LEN as f64;
        let variance = returns
            .iter()
            .map(|value| {
                let difference = *value - mean;
                difference * difference
            })
            .sum::<f64>()
            / HISTORY_LEN as f64;
        let sigma30 = variance.sqrt();
        if !sigma30.is_finite() {
            return None;
        }

        let r30 = close_raw as f64 / p0_raw as f64 - 1.0;
        let required_drop = 0.012_f64.max(3.0 * sigma30);
        if r30 > -required_drop {
            return None;
        }

        let spread = i128::from(p0_raw) - i128::from(low_raw);
        let recovery_raw = i128::from(low_raw) + spread * 2 / 5;
        let recovery_raw = i64::try_from(recovery_raw).ok()?;
        Some(Price::from_raw(recovery_raw))
    }

    fn cooldown_active(&self, now_ns: i64) -> bool {
        let Some(last_entry_ns) = self.last_entry_ns else {
            return false;
        };
        now_ns.saturating_sub(last_entry_ns) < TimeDelta::from_hours(12).as_nanos()
    }
}

impl Strategy for LiquidationFlushRebound {
    fn name(&self) -> String {
        "Liquidation-Flush Rebound".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let minute = bar.close_ts.as_nanos().div_euclid(minute_ns);
        self.push_minute(minute, bar);
        self.push_bucket_bar(minute, bar);

        if self.pending_order && ctx.open_orders() == 0 {
            self.pending_order = false;
        }
        if ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            self.watch = None;
            let matured = self.entry_fill_ns.is_some_and(|entry_ns| {
                bar.close_ts.as_nanos().saturating_sub(entry_ns)
                    >= TimeDelta::from_mins(240).as_nanos()
            });
            if matured {
                let volume_cap = bar.volume.mul_ratio(1, 10);
                let qty = if position < volume_cap {
                    position
                } else {
                    volume_cap
                };
                if qty.is_positive() && ctx.open_orders() == 0 {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
                    self.pending_order = true;
                }
            }
            return;
        }
        if !position.is_zero() {
            self.watch = None;
            return;
        }
        self.entry_fill_ns = None;

        let now_ns = bar.close_ts.as_nanos();
        let mut armed_this_bar = false;
        if let Some(recovery_level) = self.shock_recovery_level(minute) {
            self.watch = Some(Watch {
                recovery_level,
                expires_ns: now_ns.saturating_add(TimeDelta::from_mins(60).as_nanos()),
            });
            armed_this_bar = true;
        }

        if let Some(watch) = self.watch {
            if now_ns > watch.expires_ns {
                self.watch = None;
            } else if !armed_this_bar
                && !self.cooldown_active(now_ns)
                && bar.close >= watch.recovery_level
            {
                let sizing_cash = ctx.cash().mul_ratio(95, 100);
                if sizing_cash.is_positive() {
                    let affordable = ctx.affordable_qty(
                        INSTRUMENT,
                        bar.close,
                        sizing_cash,
                        SIZING_MARGIN_PPM,
                    );
                    let volume_cap = bar.volume.mul_ratio(1, 10);
                    let qty = if affordable < volume_cap {
                        affordable
                    } else {
                        volume_cap
                    };
                    if qty.is_positive() && ctx.open_orders() == 0 {
                        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                        self.pending_order = true;
                        self.watch = None;
                    }
                }
            }
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.side == Side::Buy {
            if self.entry_fill_ns.is_none() {
                self.entry_fill_ns = Some(fill.ts.as_nanos());
            }
            self.last_entry_ns = Some(fill.ts.as_nanos());
            self.watch = None;
        } else if fill.side == Side::Sell && ctx.position(INSTRUMENT).is_zero() {
            self.entry_fill_ns = None;
        }
    }

    fn on_finish(&mut self, ctx: &mut Ctx) {
        let position = ctx.position(INSTRUMENT);
        if position.is_positive() && ctx.open_orders() == 0 {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
        }
    }
}

export_strategy!(LiquidationFlushRebound::new());
