use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta, Timestamp,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const VOLUME_WINDOW: usize = 30;
const BASELINE_WINDOW: usize = 1_440;
const MAX_SETUP_AGE: u16 = 90;
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy)]
enum Phase {
    Idle,
    Setup {
        low_raw: i64,
        age: u16,
    },
    Entry {
        low_raw: i64,
        remaining_raw: i64,
        entry_ns: Option<i64>,
    },
    Long {
        low_raw: i64,
        entry_ns: i64,
    },
    Exit {
        low_raw: i64,
    },
}

struct CascadeReclaim {
    phase: Phase,
    closes: VecDeque<i64>,
    lows: VecDeque<i64>,
    volumes: VecDeque<i64>,
    volume_sum: i128,
    prior_volume_windows: VecDeque<i128>,
}

impl CascadeReclaim {
    fn new() -> Self {
        Self {
            phase: Phase::Idle,
            closes: VecDeque::with_capacity(VOLUME_WINDOW + 1),
            lows: VecDeque::with_capacity(VOLUME_WINDOW),
            volumes: VecDeque::with_capacity(VOLUME_WINDOW),
            volume_sum: 0,
            prior_volume_windows: VecDeque::with_capacity(BASELINE_WINDOW),
        }
    }

    // Returns the event low if the current completed bar satisfies the setup filter.
    fn update_history(&mut self, bar: &Bar) -> Option<i64> {
        self.closes.push_back(bar.close.raw());
        if self.closes.len() > VOLUME_WINDOW + 1 {
            self.closes.pop_front();
        }

        self.lows.push_back(bar.low.raw());
        if self.lows.len() > VOLUME_WINDOW {
            self.lows.pop_front();
        }

        self.volumes.push_back(bar.volume.raw());
        self.volume_sum += i128::from(bar.volume.raw());
        if self.volumes.len() > VOLUME_WINDOW {
            if let Some(old_volume) = self.volumes.pop_front() {
                self.volume_sum -= i128::from(old_volume);
            }
        }

        let current_window_volume = if self.volumes.len() == VOLUME_WINDOW {
            Some(self.volume_sum)
        } else {
            None
        };

        let volume_is_extreme = if let Some(current_volume) = current_window_volume {
            let extreme = if self.prior_volume_windows.len() == BASELINE_WINDOW {
                let mut sorted: Vec<i128> = self.prior_volume_windows.iter().copied().collect();
                sorted.sort_unstable();
                let middle_sum = sorted[BASELINE_WINDOW / 2 - 1]
                    + sorted[BASELINE_WINDOW / 2];
                current_volume * 2 >= middle_sum
            } else {
                false
            };

            self.prior_volume_windows.push_back(current_volume);
            if self.prior_volume_windows.len() > BASELINE_WINDOW {
                self.prior_volume_windows.pop_front();
            }
            extreme
        } else {
            false
        };

        if !volume_is_extreme || self.closes.len() != VOLUME_WINDOW + 1 {
            return None;
        }

        let old_close = *self.closes.front()?;
        if old_close <= 0 {
            return None;
        }
        // Current close is at least 1.5% below the close 30 minutes earlier.
        if i128::from(bar.close.raw()) * 10_000 > i128::from(old_close) * 9_850 {
            return None;
        }

        self.lows.iter().copied().min()
    }

    fn maybe_arm(&mut self, ctx: &Ctx, signal_low: Option<i64>) {
        if !matches!(self.phase, Phase::Idle)
            || !ctx.position(INSTRUMENT).is_zero()
            || ctx.open_orders() != 0
        {
            return;
        }
        if let Some(low_raw) = signal_low {
            self.phase = Phase::Setup { low_raw, age: 0 };
        }
    }

    fn submit_entry_clip(&self, ctx: &mut Ctx, bar: &Bar) {
        let Phase::Entry { remaining_raw, .. } = self.phase else {
            return;
        };
        if remaining_raw <= 0 || ctx.open_orders() != 0 {
            return;
        }

        let cap_raw = bar.volume.mul_ratio(1, 10).raw();
        let affordable = ctx.affordable_qty(
            INSTRUMENT,
            bar.close,
            ctx.cash(),
            SIZING_MARGIN_PPM,
        );
        let qty_raw = remaining_raw.min(cap_raw).min(affordable.raw());
        if qty_raw > 0 {
            ctx.submit(OrderRequest::market(
                INSTRUMENT,
                Side::Buy,
                Qty::from_raw(qty_raw),
            ));
        }
    }

    fn submit_exit_clip(&self, ctx: &mut Ctx, bar: &Bar) {
        if ctx.open_orders() != 0 {
            return;
        }
        let position = ctx.position(INSTRUMENT);
        if !position.is_positive() {
            return;
        }

        let cap_raw = bar.volume.mul_ratio(1, 10).raw();
        let qty_raw = position.raw().min(cap_raw);
        if qty_raw > 0 {
            ctx.submit(OrderRequest::market(
                INSTRUMENT,
                Side::Sell,
                Qty::from_raw(qty_raw),
            ));
        }
    }
}

impl Strategy for CascadeReclaim {
    fn name(&self) -> String {
        "Cascade Reclaim".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let signal_low = self.update_history(bar);

        match self.phase {
            Phase::Idle => self.maybe_arm(ctx, signal_low),
            Phase::Setup { low_raw, age } => {
                let next_age = age.saturating_add(1);
                // Reclaim the recorded event low by at least 0.6%.
                let confirmed = i128::from(bar.close.raw()) * 1_000
                    >= i128::from(low_raw) * 1_006;

                if confirmed {
                    let qty = ctx.affordable_qty(
                        INSTRUMENT,
                        bar.close,
                        ctx.cash(),
                        SIZING_MARGIN_PPM,
                    );
                    if qty.is_positive() {
                        self.phase = Phase::Entry {
                            low_raw,
                            remaining_raw: qty.raw(),
                            entry_ns: None,
                        };
                        self.submit_entry_clip(ctx, bar);
                    } else {
                        self.phase = Phase::Idle;
                    }
                } else if next_age >= MAX_SETUP_AGE {
                    self.phase = Phase::Idle;
                    self.maybe_arm(ctx, signal_low);
                } else {
                    self.phase = Phase::Setup {
                        low_raw,
                        age: next_age,
                    };
                }
            }
            Phase::Entry { .. } => self.submit_entry_clip(ctx, bar),
            Phase::Long { low_raw, entry_ns } => {
                let stop_hit = i128::from(bar.close.raw()) * 1_000
                    < i128::from(low_raw) * 998;
                let elapsed = i128::from(bar.close_ts.as_nanos()) - i128::from(entry_ns);
                let time_exit = elapsed >= i128::from(TimeDelta::from_hours(8).as_nanos());

                if stop_hit || time_exit {
                    self.phase = Phase::Exit { low_raw };
                    self.submit_exit_clip(ctx, bar);
                }
            }
            Phase::Exit { .. } => {
                if ctx.position(INSTRUMENT).is_zero() && ctx.open_orders() == 0 {
                    self.phase = Phase::Idle;
                    self.maybe_arm(ctx, signal_low);
                } else {
                    self.submit_exit_clip(ctx, bar);
                }
            }
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        match (self.phase, fill.side) {
            (
                Phase::Entry {
                    low_raw,
                    remaining_raw,
                    entry_ns,
                },
                Side::Buy,
            ) => {
                let remaining = remaining_raw.saturating_sub(fill.qty.raw()).max(0);
                let first_entry_ns = entry_ns.or(Some(fill.ts.as_nanos()));
                if remaining == 0 {
                    self.phase = Phase::Long {
                        low_raw,
                        entry_ns: first_entry_ns.unwrap_or(fill.ts.as_nanos()),
                    };
                } else {
                    self.phase = Phase::Entry {
                        low_raw,
                        remaining_raw: remaining,
                        entry_ns: first_entry_ns,
                    };
                }
            }
            (Phase::Exit { .. }, Side::Sell) => {
                if ctx.position(INSTRUMENT).is_zero() && ctx.open_orders() == 0 {
                    self.phase = Phase::Idle;
                }
            }
            _ => {}
        }
    }
}

export_strategy!(CascadeReclaim::new());
