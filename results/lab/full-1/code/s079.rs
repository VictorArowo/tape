use std::collections::{BTreeMap, VecDeque};

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const PROFILE_BARS: usize = 1_440;
const POC_LAG_BARS: usize = 360;
const SIZING_MARGIN_PPM: i64 = 5_000;
const RAW_SCALE: f64 = 100_000_000.0;

fn bin_log_width() -> f64 {
    1.0025_f64.ln()
}

#[derive(Clone, Copy)]
struct ProfileEntry {
    bin: Option<i64>,
    volume: i128,
}

struct VolumeProfile {
    bars: VecDeque<ProfileEntry>,
    volumes: BTreeMap<i64, i128>,
}

impl VolumeProfile {
    fn new() -> Self {
        Self {
            bars: VecDeque::with_capacity(PROFILE_BARS + 1),
            volumes: BTreeMap::new(),
        }
    }

    fn poc(&self) -> Option<i64> {
        if self.bars.len() != PROFILE_BARS {
            return None;
        }

        // BTreeMap iteration makes ties deterministic: retain the lower bin.
        let mut best: Option<(i64, i128)> = None;
        for (&bin, &volume) in &self.volumes {
            if best.map_or(true, |(_, best_volume)| volume > best_volume) {
                best = Some((bin, volume));
            }
        }
        let (bin, _) = best?;
        let midpoint = ((bin as f64 + 0.5) * bin_log_width()).exp();
        let raw = midpoint * RAW_SCALE;
        if !raw.is_finite() || raw <= 0.0 || raw > i64::MAX as f64 {
            return None;
        }
        Some(raw.round() as i64)
    }

    fn push(&mut self, entry: ProfileEntry) {
        if let (Some(bin), volume) = (entry.bin, entry.volume) {
            if volume > 0 {
                *self.volumes.entry(bin).or_insert(0) += volume;
            }
        }
        self.bars.push_back(entry);

        if self.bars.len() > PROFILE_BARS {
            if let Some(old) = self.bars.pop_front() {
                if let (Some(bin), volume) = (old.bin, old.volume) {
                    if volume > 0 {
                        let remove_bin = if let Some(total) = self.volumes.get_mut(&bin) {
                            *total -= volume;
                            *total <= 0
                        } else {
                            false
                        };
                        if remove_bin {
                            self.volumes.remove(&bin);
                        }
                    }
                }
            }
        }
    }
}

fn bar_bin(bar: &Bar) -> Option<i64> {
    let typical = (bar.high.to_f64() + bar.low.to_f64() + bar.close.to_f64()) / 3.0;
    if !typical.is_finite() || typical <= 0.0 {
        return None;
    }
    let bin = (typical.ln() / bin_log_width()).floor();
    if !bin.is_finite() || bin < i64::MIN as f64 || bin > i64::MAX as f64 {
        return None;
    }
    Some(bin as i64)
}

fn rose_at_least_half_percent(current: i64, reference: i64) -> bool {
    i128::from(current) * 1_000 >= i128::from(reference) * 1_005
}

fn fell_at_least_half_percent(current: i64, reference: i64) -> bool {
    i128::from(current) * 1_000 <= i128::from(reference) * 995
}

struct VolumeProfileMigration {
    profile: VolumeProfile,
    poc_history: VecDeque<Option<i64>>,
    previous_close: Option<i64>,
    previous_poc: Option<i64>,
    entry_pending: bool,
    entry_target_raw: i64,
    exit_pending: bool,
}

impl VolumeProfileMigration {
    fn volume_cap_raw(bar: &Bar) -> i64 {
        if bar.volume.raw() > 0 {
            bar.volume.raw() / 10
        } else {
            0
        }
    }

    fn submit_entry_chunk(&self, ctx: &mut Ctx, bar: &Bar) {
        if ctx.open_orders() != 0 {
            return;
        }
        let remaining = self.entry_target_raw - ctx.position(INSTRUMENT).raw();
        if remaining <= 0 {
            return;
        }
        let affordable = ctx
            .affordable_qty(INSTRUMENT, bar.close, ctx.cash(), SIZING_MARGIN_PPM)
            .raw();
        let qty_raw = remaining.min(affordable).min(Self::volume_cap_raw(bar));
        if qty_raw > 0 {
            ctx.submit(OrderRequest::market(
                INSTRUMENT,
                Side::Buy,
                Qty::from_raw(qty_raw),
            ));
        }
    }

    fn submit_exit_chunk(&self, ctx: &mut Ctx, bar: &Bar) {
        if ctx.open_orders() != 0 {
            return;
        }
        let position_raw = ctx.position(INSTRUMENT).raw();
        let qty_raw = position_raw.min(Self::volume_cap_raw(bar));
        if qty_raw > 0 {
            ctx.submit(OrderRequest::market(
                INSTRUMENT,
                Side::Sell,
                Qty::from_raw(qty_raw),
            ));
        }
    }
}

impl Strategy for VolumeProfileMigration {
    fn name(&self) -> String {
        "volume-profile-migration".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        // The profile has only prior completed bars here, so the current bar is excluded.
        let current_poc = self.profile.poc();
        let poc_6h = if self.poc_history.len() >= POC_LAG_BARS {
            self.poc_history
                .iter()
                .rev()
                .nth(POC_LAG_BARS - 1)
                .copied()
                .flatten()
        } else {
            None
        };

        let entry_signal = match (
            self.previous_close,
            self.previous_poc,
            current_poc,
            poc_6h,
        ) {
            (Some(previous_close), Some(previous_poc), Some(poc), Some(old_poc)) => {
                previous_close <= previous_poc
                    && bar.close.raw() > poc
                    && rose_at_least_half_percent(poc, old_poc)
            }
            _ => false,
        };
        let exit_signal = match (current_poc, poc_6h) {
            (Some(poc), Some(old_poc)) => {
                fell_at_least_half_percent(bar.close.raw(), poc)
                    || fell_at_least_half_percent(poc, old_poc)
            }
            _ => false,
        };

        // Save this bar's prior-bar POC before rolling the current bar into the profile.
        self.poc_history.push_back(current_poc);
        if self.poc_history.len() > POC_LAG_BARS {
            self.poc_history.pop_front();
        }
        self.previous_close = Some(bar.close.raw());
        self.previous_poc = current_poc;
        self.profile.push(ProfileEntry {
            bin: bar_bin(bar),
            volume: i128::from(bar.volume.raw().max(0)),
        });

        if ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);

        if self.entry_pending {
            if exit_signal {
                self.entry_pending = false;
                if position.is_positive() {
                    self.exit_pending = true;
                } else {
                    return;
                }
            } else if position.raw() >= self.entry_target_raw {
                self.entry_pending = false;
                return;
            } else {
                self.submit_entry_chunk(ctx, bar);
                return;
            }
        }

        if self.exit_pending {
            if !position.is_positive() {
                self.exit_pending = false;
                return;
            }
            self.submit_exit_chunk(ctx, bar);
            return;
        }

        if position.is_positive() {
            if exit_signal {
                self.exit_pending = true;
                self.submit_exit_chunk(ctx, bar);
            }
        } else if entry_signal {
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                self.entry_target_raw = qty.raw();
                self.entry_pending = true;
                self.submit_entry_chunk(ctx, bar);
            }
        }
    }
}

export_strategy!(VolumeProfileMigration {
    profile: VolumeProfile::new(),
    poc_history: VecDeque::with_capacity(POC_LAG_BARS),
    previous_close: None,
    previous_poc: None,
    entry_pending: false,
    entry_target_raw: 0,
    exit_pending: false,
});
