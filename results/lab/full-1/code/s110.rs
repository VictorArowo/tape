use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SCALE: i128 = 100_000_000;
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy)]
struct DayStats {
    high: Price,
    low: Price,
}

#[derive(Clone, Copy)]
struct Setup {
    sweep_low: Price,
    level: Price,
    target: Price,
}

struct PriorDayLowSweep {
    current_day: Option<i64>,
    current_stats: Option<DayStats>,
    previous_stats: Option<DayStats>,
    last_entry_day: Option<i64>,
    pending_setup: Option<Setup>,
    active_setup: Option<Setup>,
    entry_time_ns: Option<i64>,
}

impl PriorDayLowSweep {
    fn new() -> Self {
        Self {
            current_day: None,
            current_stats: None,
            previous_stats: None,
            last_entry_day: None,
            pending_setup: None,
            active_setup: None,
            entry_time_ns: None,
        }
    }

    fn update_daily_stats(&mut self, day: i64, bar: &Bar) {
        match self.current_day {
            Some(current_day) if current_day == day => {
                if let Some(stats) = &mut self.current_stats {
                    if bar.high > stats.high {
                        stats.high = bar.high;
                    }
                    if bar.low < stats.low {
                        stats.low = bar.low;
                    }
                }
            }
            Some(current_day) => {
                self.previous_stats = if current_day.checked_add(1) == Some(day) {
                    self.current_stats
                } else {
                    None
                };
                self.current_day = Some(day);
                self.current_stats = Some(DayStats {
                    high: bar.high,
                    low: bar.low,
                });
            }
            None => {
                self.current_day = Some(day);
                self.current_stats = Some(DayStats {
                    high: bar.high,
                    low: bar.low,
                });
            }
        }
    }

    fn signal_setup(&self, bar: &Bar) -> Option<Setup> {
        let stats = self.previous_stats?;
        let level_raw = i128::from(stats.low.raw());
        if level_raw <= 0 || bar.close <= stats.low {
            return None;
        }

        let penetration = level_raw - i128::from(bar.low.raw());
        if penetration <= 0
            || penetration * 100_000 < level_raw * 50
            || penetration * 100_000 > level_raw * 500
        {
            return None;
        }

        let range_raw = i128::from(stats.high.raw()) - level_raw;
        if range_raw <= 0 {
            return None;
        }
        let target_raw = level_raw + range_raw / 2;
        let target = Price::from_raw(i64::try_from(target_raw).ok()?);

        Some(Setup {
            sweep_low: bar.low,
            level: stats.low,
            target,
        })
    }

    fn entry_qty(&self, ctx: &Ctx, bar: &Bar) -> Qty {
        if !bar.close.is_positive() || !bar.volume.is_positive() {
            return Qty::from_raw(0);
        }

        let equity = ctx.equity();
        if !equity.is_positive() {
            return Qty::from_raw(0);
        }

        // Target 25% of equity at the signal bar's close; affordability and
        // the signal bar's observed volume further limit the order size.
        let target_raw = i128::from(equity.raw()) * SCALE * 25
            / 100
            / i128::from(bar.close.raw());
        let volume_cap_raw = i128::from(bar.volume.raw()) / 10;
        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        let qty_raw = target_raw
            .min(volume_cap_raw)
            .min(i128::from(affordable.raw()));

        match i64::try_from(qty_raw) {
            Ok(raw) if raw > 0 => Qty::from_raw(raw),
            _ => Qty::from_raw(0),
        }
    }
}

impl Strategy for PriorDayLowSweep {
    fn name(&self) -> String {
        "prior-day-low-sweep".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let day_ns = TimeDelta::from_days(1).as_nanos();
        let day = bar.close_ts.as_nanos().div_euclid(day_ns);
        self.update_daily_stats(day, bar);

        // A completed but unfilled IOC entry is no longer pending.
        if self.pending_setup.is_some() && ctx.open_orders() == 0 {
            self.pending_setup = None;
        }

        if let Some(setup) = self.active_setup {
            let position = ctx.position(INSTRUMENT);
            if !position.is_positive() {
                self.active_setup = None;
                self.entry_time_ns = None;
                return;
            }

            let elapsed = self
                .entry_time_ns
                .map(|entry_ns| bar.close_ts.as_nanos().saturating_sub(entry_ns))
                .unwrap_or(0);
            let timed_exit = elapsed >= TimeDelta::from_hours(6).as_nanos();
            let price_exit = bar.close < setup.sweep_low || bar.close >= setup.target;

            if (price_exit || timed_exit) && ctx.open_orders() == 0 {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            }
            return;
        }

        if self.pending_setup.is_some()
            || ctx.open_orders() != 0
            || ctx.position(INSTRUMENT).is_positive()
            || self.last_entry_day == Some(day)
        {
            return;
        }

        let Some(setup) = self.signal_setup(bar) else {
            return;
        };
        let qty = self.entry_qty(ctx, bar);
        if qty.is_positive() {
            // A market order submitted after this close is intended to fill at
            // the following bar's open, not on the signal bar.
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            self.pending_setup = Some(setup);
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        match fill.side {
            Side::Buy => {
                if let Some(setup) = self.pending_setup.take() {
                    self.active_setup = Some(setup);
                    self.entry_time_ns = Some(fill.ts.as_nanos());
                    let day_ns = TimeDelta::from_days(1).as_nanos();
                    self.last_entry_day = Some(fill.ts.as_nanos().div_euclid(day_ns));
                }
            }
            Side::Sell => {
                if ctx.position(INSTRUMENT).is_zero() {
                    self.active_setup = None;
                    self.entry_time_ns = None;
                }
            }
        }
    }
}

export_strategy!(PriorDayLowSweep::new());