use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const BREAKOUT_LOOKBACK: usize = 1_440;
const EXIT_LOOKBACK: usize = 720;
const HOLD_LIMIT: usize = 720;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct VolumeBreakout {
    history: VecDeque<(i64, i64, i64)>, // high, low, volume
    bars_in_position: usize,
}

impl VolumeBreakout {
    fn new() -> Self {
        Self {
            history: VecDeque::with_capacity(BREAKOUT_LOOKBACK),
            bars_in_position: 0,
        }
    }

    fn preceding_high(&self, count: usize) -> Option<i64> {
        (self.history.len() >= count).then(|| {
            self.history
                .iter()
                .rev()
                .take(count)
                .map(|bar| bar.0)
                .max()
                .expect("non-empty lookback")
        })
    }

    fn preceding_low(&self, count: usize) -> Option<i64> {
        (self.history.len() >= count).then(|| {
            self.history
                .iter()
                .rev()
                .take(count)
                .map(|bar| bar.1)
                .min()
                .expect("non-empty lookback")
        })
    }

    // Returns twice the median so comparisons remain exact for even-sized windows.
    fn twice_median_volume(&self) -> Option<i128> {
        if self.history.len() < BREAKOUT_LOOKBACK {
            return None;
        }
        let mut volumes: Vec<i64> = self.history.iter().map(|bar| bar.2).collect();
        volumes.sort_unstable();
        let middle = volumes.len() / 2;
        Some(if volumes.len() % 2 == 0 {
            i128::from(volumes[middle - 1]) + i128::from(volumes[middle])
        } else {
            i128::from(volumes[middle]) * 2
        })
    }
}

impl Strategy for VolumeBreakout {
    fn name(&self) -> String {
        "Volume-Confirmed Breakout".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            self.bars_in_position = self.bars_in_position.saturating_add(1);
            if ctx.open_orders() == 0 {
                let breakdown = self
                    .preceding_low(EXIT_LOOKBACK)
                    .is_some_and(|low| bar.close.raw() < low);
                if breakdown || self.bars_in_position >= HOLD_LIMIT {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                }
            }
        } else {
            self.bars_in_position = 0;
            if ctx.open_orders() == 0 {
                if let (Some(prior_high), Some(twice_median)) = (
                    self.preceding_high(BREAKOUT_LOOKBACK),
                    self.twice_median_volume(),
                ) {
                    // volume > 1.5 * median, expressed without rounding the median.
                    let volume_confirmed = i128::from(bar.volume.raw()) * 8 > twice_median * 3;
                    if bar.close.raw() > prior_high && volume_confirmed {
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

        self.history
            .push_back((bar.high.raw(), bar.low.raw(), bar.volume.raw()));
        if self.history.len() > BREAKOUT_LOOKBACK {
            self.history.pop_front();
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.side == Side::Buy {
            self.bars_in_position = 0;
        } else if !ctx.position(INSTRUMENT).is_positive() {
            self.bars_in_position = 0;
        }
    }
}

export_strategy!(VolumeBreakout::new());
