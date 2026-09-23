use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const LOOKBACK: usize = 720;
const MARGIN_PPM: i64 = 5_000;

struct Hour {
    turnover: f64,
    range_fraction: f64,
    close_location: f64,
    open_raw: i64,
    close_raw: i64,
    low_raw: i64,
}

struct HourAgg {
    bucket: i64,
    count: u32,
    turnover: f64,
    open_raw: i64,
    high_raw: i64,
    low_raw: i64,
    close_raw: i64,
}

impl HourAgg {
    fn new(bucket: i64) -> Self {
        Self {
            bucket,
            count: 0,
            turnover: 0.0,
            open_raw: 0,
            high_raw: 0,
            low_raw: 0,
            close_raw: 0,
        }
    }

    fn add(&mut self, bar: &Bar) {
        if self.count == 0 {
            self.open_raw = bar.open.raw();
            self.high_raw = bar.high.raw();
            self.low_raw = bar.low.raw();
        } else {
            self.high_raw = self.high_raw.max(bar.high.raw());
            self.low_raw = self.low_raw.min(bar.low.raw());
        }
        self.close_raw = bar.close.raw();
        self.turnover += (bar.close * bar.volume).to_f64();
        self.count = self.count.saturating_add(1);
    }

    fn finish(self) -> Option<Hour> {
        if self.count < 60 {
            return None;
        }
        let span = (i128::from(self.high_raw) - i128::from(self.low_raw)) as f64;
        let range_fraction = if self.open_raw > 0 {
            span / self.open_raw as f64
        } else {
            0.0
        };
        let close_location = if span > 0.0 {
            (i128::from(self.close_raw) - i128::from(self.low_raw)) as f64 / span
        } else {
            0.0
        };
        Some(Hour {
            turnover: self.turnover,
            range_fraction,
            close_location,
            open_raw: self.open_raw,
            close_raw: self.close_raw,
            low_raw: self.low_raw,
        })
    }
}

struct HourBuilder {
    current: Option<HourAgg>,
}

impl HourBuilder {
    fn new() -> Self {
        Self { current: None }
    }

    fn push(&mut self, bar: &Bar) -> Vec<Hour> {
        let hour_ns = TimeDelta::from_hours(1).as_nanos();
        let bucket = bar.open_ts.as_nanos().div_euclid(hour_ns);
        let mut completed = Vec::new();

        if self.current.as_ref().is_some_and(|hour| hour.bucket != bucket) {
            if let Some(hour) = self.current.take().and_then(HourAgg::finish) {
                completed.push(hour);
            }
        }
        if self.current.is_none() {
            self.current = Some(HourAgg::new(bucket));
        }
        if let Some(current) = self.current.as_mut() {
            current.add(bar);
        }

        // A minute ending at the UTC hour boundary completes the hour immediately.
        if bar.close_ts.as_nanos().div_euclid(hour_ns) > bucket {
            if let Some(hour) = self.current.take().and_then(HourAgg::finish) {
                completed.push(hour);
            }
        }
        completed
    }
}

#[derive(Clone, Copy)]
struct HourStat {
    turnover: f64,
    range_fraction: f64,
}

struct TurnoverAbsorption {
    hours: HourBuilder,
    history: VecDeque<HourStat>,
    entry_order_pending: bool,
    entry_time_ns: Option<i64>,
    setup_low_raw: Option<i64>,
    exit_active: bool,
}

impl TurnoverAbsorption {
    fn new() -> Self {
        Self {
            hours: HourBuilder::new(),
            history: VecDeque::with_capacity(LOOKBACK + 1),
            entry_order_pending: false,
            entry_time_ns: None,
            setup_low_raw: None,
            exit_active: false,
        }
    }

    fn thresholds(&self) -> Option<(f64, f64)> {
        if self.history.len() < LOOKBACK {
            return None;
        }
        let mut turnovers: Vec<f64> = self.history.iter().map(|x| x.turnover).collect();
        turnovers.sort_by(|a, b| a.total_cmp(b));
        let nearest_rank = (9 * LOOKBACK + 9) / 10;
        let turnover_90 = turnovers[nearest_rank - 1];

        let mut ranges: Vec<f64> = self.history.iter().map(|x| x.range_fraction).collect();
        ranges.sort_by(|a, b| a.total_cmp(b));
        let mid = LOOKBACK / 2;
        let median_range = (ranges[mid - 1] + ranges[mid]) / 2.0;
        Some((turnover_90, median_range))
    }

    // Compare the new hour with history before adding it, so it never enters its own thresholds.
    fn record_hour(&mut self, hour: &Hour) -> bool {
        let signal = self.thresholds().is_some_and(|(turnover_90, median_range)| {
            hour.turnover >= turnover_90
                && hour.range_fraction <= median_range
                && hour.close_location >= 0.75
                && hour.close_raw > hour.open_raw
        });

        self.history.push_back(HourStat {
            turnover: hour.turnover,
            range_fraction: hour.range_fraction,
        });
        if self.history.len() > LOOKBACK {
            self.history.pop_front();
        }
        signal
    }

    fn participation_cap(bar: &Bar) -> Qty {
        bar.volume.mul_ratio(1, 10)
    }
}

impl Strategy for TurnoverAbsorption {
    fn name(&self) -> String {
        "turnover-absorption".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let mut position = ctx.position(INSTRUMENT);
        if self.entry_order_pending && ctx.open_orders() == 0 && position.is_zero() {
            self.entry_order_pending = false;
            self.setup_low_raw = None;
        }

        let mut entry_candidate = None;
        for hour in self.hours.push(bar) {
            let signal = self.record_hour(&hour);
            if self
                .setup_low_raw
                .is_some_and(|setup_low| hour.close_raw < setup_low)
            {
                self.exit_active = true;
            }
            if signal && entry_candidate.is_none() {
                entry_candidate = Some(hour.low_raw);
            }
        }

        position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            if let Some(entry_ns) = self.entry_time_ns {
                let elapsed = bar.close_ts.as_nanos().saturating_sub(entry_ns);
                if elapsed >= TimeDelta::from_hours(12).as_nanos() {
                    self.exit_active = true;
                }
            }
        } else if !self.entry_order_pending {
            self.entry_time_ns = None;
            self.setup_low_raw = None;
            self.exit_active = false;
        }

        if position.is_positive() && self.exit_active {
            if ctx.open_orders() == 0 {
                let cap = Self::participation_cap(bar);
                let qty_raw = position.raw().min(cap.raw());
                let qty = Qty::from_raw(qty_raw);
                if qty.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
                }
            }
            return;
        }

        if position.is_zero()
            && !self.entry_order_pending
            && !self.exit_active
            && ctx.open_orders() == 0
        {
            if let Some(setup_low) = entry_candidate {
                let affordable = ctx.affordable_qty(
                    INSTRUMENT,
                    bar.close,
                    ctx.cash(),
                    MARGIN_PPM,
                );
                let cap = Self::participation_cap(bar);
                let qty = Qty::from_raw(affordable.raw().min(cap.raw()));
                if qty.is_positive() {
                    self.setup_low_raw = Some(setup_low);
                    self.entry_order_pending = true;
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                }
            }
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.side == Side::Buy {
            self.entry_order_pending = false;
            if self.entry_time_ns.is_none() {
                self.entry_time_ns = Some(fill.ts.as_nanos());
            }
        }
    }
}

export_strategy!(TurnoverAbsorption::new());
