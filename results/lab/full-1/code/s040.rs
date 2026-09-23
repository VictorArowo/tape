use tape_guest::{
    export_strategy, Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const RAW_SCALE: i128 = 100_000_000;

struct WeeklyBandRebalancer;

impl Strategy for WeeklyBandRebalancer {
    fn name(&self) -> String {
        "weekly-band-rebalancer".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let timestamp_ns = bar.close_ts.as_nanos();
        let minute_of_day = (timestamp_ns / minute_ns).rem_euclid(1440);
        let day_of_week = (timestamp_ns / day_ns).rem_euclid(7);

        // Day 4 is Monday when Thursday is day 0.
        if minute_of_day != 0 || day_of_week != 4 || ctx.open_orders() != 0 {
            return;
        }

        if !bar.close.is_positive() {
            return;
        }

        let equity_raw = ctx.equity().raw();
        if equity_raw <= 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        let btc_value_raw = (bar.close * position).raw();
        let btc_value_scaled = i128::from(btc_value_raw) * 100;
        let equity_scaled = i128::from(equity_raw);

        // Rebalance only outside the inclusive 57%-63% band.
        if btc_value_scaled >= equity_scaled * 57
            && btc_value_scaled <= equity_scaled * 63
        {
            return;
        }

        // Convert the 60%-of-equity target value to raw quantity units.
        let target_qty_raw = (equity_scaled * 60 * RAW_SCALE)
            / (i128::from(bar.close.raw()) * 100);
        let Ok(target_qty_raw) = i64::try_from(target_qty_raw) else {
            return;
        };
        let target_qty = Qty::from_raw(target_qty_raw);

        if target_qty > position {
            let needed = target_qty - position;
            let affordable = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            let qty = if affordable < needed {
                affordable
            } else {
                needed
            };
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        } else if target_qty < position {
            let qty = position - target_qty;
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
            }
        }
    }
}

export_strategy!(WeeklyBandRebalancer);
