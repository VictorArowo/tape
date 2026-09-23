use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const WINDOW_BARS: usize = 360;
const ENTRY_THRESHOLD: f64 = 0.10;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct FlowWindow {
    bars: VecDeque<(f64, f64)>,
    signed_volume: f64,
    total_volume: f64,
}

impl FlowWindow {
    fn new() -> Self {
        Self {
            bars: VecDeque::with_capacity(WINDOW_BARS + 1),
            signed_volume: 0.0,
            total_volume: 0.0,
        }
    }

    fn push(&mut self, bar: &Bar) {
        let volume = bar.volume.to_f64();
        let signed = if bar.high.raw() == bar.low.raw() {
            0.0
        } else {
            let high = bar.high.to_f64();
            let low = bar.low.to_f64();
            let close = bar.close.to_f64();
            volume * ((2.0 * close - high - low) / (high - low))
        };

        self.bars.push_back((signed, volume));
        self.signed_volume += signed;
        self.total_volume += volume;

        if self.bars.len() > WINDOW_BARS {
            if let Some((old_signed, old_volume)) = self.bars.pop_front() {
                self.signed_volume -= old_signed;
                self.total_volume -= old_volume;
            }
        }
    }

    fn imbalance(&self) -> Option<f64> {
        if self.bars.len() != WINDOW_BARS {
            return None;
        }
        if self.total_volume > 0.0 {
            Some(self.signed_volume / self.total_volume)
        } else {
            Some(0.0)
        }
    }
}

#[derive(Clone, Copy)]
enum PlannedSide {
    Buy,
    Sell,
}

struct PlannedOrder {
    side: PlannedSide,
    remaining: Qty,
}

struct CandleWeightedFlowImbalance {
    flow: FlowWindow,
    plan: Option<PlannedOrder>,
}

impl CandleWeightedFlowImbalance {
    fn new() -> Self {
        Self {
            flow: FlowWindow::new(),
            plan: None,
        }
    }
}

fn min_qty(a: Qty, b: Qty) -> Qty {
    Qty::from_raw(a.raw().min(b.raw()))
}

impl Strategy for CandleWeightedFlowImbalance {
    fn name(&self) -> String {
        "candle-weighted-flow-imbalance(360)".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        self.flow.push(bar);
        let Some(imbalance) = self.flow.imbalance() else {
            return;
        };

        // Market orders are IOC in the SDK. Wait for any current attempt to finish,
        // then resubmit only its unfilled remainder on a later bar.
        if ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);

        if let Some(mut plan) = self.plan.take() {
            let keep_plan = match plan.side {
                PlannedSide::Buy => {
                    if position.is_positive() && imbalance <= 0.0 {
                        plan.side = PlannedSide::Sell;
                        plan.remaining = position;
                        plan.remaining.is_positive()
                    } else if plan.remaining.is_positive()
                        && !(position.is_zero() && imbalance < ENTRY_THRESHOLD)
                    {
                        true
                    } else {
                        false
                    }
                }
                PlannedSide::Sell => {
                    if position.is_positive() && imbalance <= 0.0 {
                        plan.remaining = min_qty(plan.remaining, position);
                        plan.remaining.is_positive()
                    } else {
                        false
                    }
                }
            };
            if keep_plan {
                self.plan = Some(plan);
            }
        }

        if self.plan.is_none() {
            if position.is_zero() && imbalance >= ENTRY_THRESHOLD {
                let qty = ctx.affordable_qty(
                    INSTRUMENT,
                    bar.close,
                    ctx.cash(),
                    SIZING_MARGIN_PPM,
                );
                if qty.is_positive() {
                    self.plan = Some(PlannedOrder {
                        side: PlannedSide::Buy,
                        remaining: qty,
                    });
                }
            } else if position.is_positive() && imbalance <= 0.0 {
                self.plan = Some(PlannedOrder {
                    side: PlannedSide::Sell,
                    remaining: position,
                });
            }
        }

        let Some(plan) = self.plan.as_ref() else {
            return;
        };

        match plan.side {
            PlannedSide::Buy => {
                let affordable = ctx.affordable_qty(
                    INSTRUMENT,
                    bar.close,
                    ctx.cash(),
                    SIZING_MARGIN_PPM,
                );
                let qty = min_qty(plan.remaining, affordable);
                if qty.is_positive() {
                    // Submitted at this close for execution at the next bar's open.
                    let _ = ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                }
            }
            PlannedSide::Sell => {
                let qty = min_qty(plan.remaining, position);
                if qty.is_positive() {
                    let _ = ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
                }
            }
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if let Some(plan) = self.plan.as_mut() {
            let remaining = plan.remaining.raw().saturating_sub(fill.qty.raw()).max(0);
            plan.remaining = Qty::from_raw(remaining);
        }
    }
}

export_strategy!(CandleWeightedFlowImbalance::new());
