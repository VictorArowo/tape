use tape_guest::{export_strategy, Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const TARGET_WEIGHT: f64 = 0.95;
const LOWER_BAND: f64 = 0.93;
const UPPER_BAND: f64 = 0.97;
const FEE_PPM: i64 = 1_000;
const SLIPPAGE: f64 = 0.0002;
const SCALE: f64 = 100_000_000.0;

struct BandRebalancing {
    initialized: bool,
    last_day: Option<i64>,
    last_close: Option<Price>,
}

impl BandRebalancing {
    fn rebalance_at(&self, ctx: &mut Ctx, reference: Price, force: bool) {
        if !reference.is_positive() || ctx.open_orders() != 0 {
            return;
        }

        let held = ctx.position(INSTRUMENT);
        let held_qty = if held.is_positive() { held } else { Qty::from_raw(0) };
        let price = reference.to_f64();
        let held_value = held_qty.to_f64() * price;
        let portfolio_value = ctx.cash().to_f64() + held_value;
        if portfolio_value <= 0.0 || !portfolio_value.is_finite() {
            return;
        }

        let weight = held_value / portfolio_value;
        if !force && weight >= LOWER_BAND && weight <= UPPER_BAND {
            return;
        }

        if held_value < TARGET_WEIGHT * portfolio_value {
            let buy_cash_factor = (1.0 + SLIPPAGE) * (1.0 + FEE_PPM as f64 / 1_000_000.0);
            let buy_cost_drag = buy_cash_factor - 1.0;
            let notional = (TARGET_WEIGHT * portfolio_value - held_value)
                / (1.0 + TARGET_WEIGHT * buy_cost_drag);
            if notional <= 0.0 {
                return;
            }

            let wanted_raw = (notional / price * SCALE).floor() as i64;
            if wanted_raw <= 0 {
                return;
            }
            let wanted = Qty::from_raw(wanted_raw);
            let affordable_price = Price::from_raw((price * (1.0 + SLIPPAGE) * SCALE).ceil() as i64);
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                affordable_price,
                ctx.cash(),
                FEE_PPM,
            );
            let qty = Qty::from_raw(wanted.raw().min(affordable.raw()));
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        } else if held_qty.is_positive() {
            let sell_net_factor = (1.0 - SLIPPAGE) * (1.0 - FEE_PPM as f64 / 1_000_000.0);
            let sell_cost_drag = 1.0 - sell_net_factor;
            let notional = (held_value - TARGET_WEIGHT * portfolio_value)
                / (1.0 - TARGET_WEIGHT * sell_cost_drag);
            if notional <= 0.0 {
                return;
            }

            let wanted_raw = (notional / price * SCALE).floor() as i64;
            if wanted_raw <= 0 {
                return;
            }
            let qty = Qty::from_raw(wanted_raw.min(held_qty.raw()));
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
            }
        }
    }
}

impl Strategy for BandRebalancing {
    fn name(&self) -> String {
        "band-rebalancing(93-97%, target=95%)".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let day_ns = TimeDelta::from_days(1).as_nanos();
        let day = bar.close_ts.as_nanos().div_euclid(day_ns);

        if !self.initialized {
            self.initialized = true;
            self.last_day = Some(day);
            self.last_close = Some(bar.close);
            // The earliest callback is after this bar closes; use its open as the initial sizing reference.
            self.rebalance_at(ctx, bar.open, true);
            return;
        }

        if self.last_day.is_some_and(|previous_day| day != previous_day) {
            if let Some(previous_close) = self.last_close {
                self.rebalance_at(ctx, previous_close, false);
            }
            self.last_day = Some(day);
        }

        self.last_close = Some(bar.close);
    }
}

export_strategy!(BandRebalancing {
    initialized: false,
    last_day: None,
    last_close: None,
});
