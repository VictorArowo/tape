use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const WINDOW_BARS: usize = 20_160;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct RollingVwap {
    bars: VecDeque<(i64, i64)>,
    weighted_price_sum: i128,
    volume_sum: i128,
}

impl RollingVwap {
    fn new() -> Self {
        Self {
            bars: VecDeque::with_capacity(WINDOW_BARS + 1),
            weighted_price_sum: 0,
            volume_sum: 0,
        }
    }

    fn push(&mut self, bar: &Bar) -> Option<Price> {
        // Use the conventional OHLC typical price for each completed bar.
        let typical_price = ((i128::from(bar.high.raw())
            + i128::from(bar.low.raw())
            + i128::from(bar.close.raw()))
            / 3) as i64;
        let volume = bar.volume.raw().max(0);

        self.bars.push_back((typical_price, volume));
        self.weighted_price_sum += i128::from(typical_price) * i128::from(volume);
        self.volume_sum += i128::from(volume);

        if self.bars.len() > WINDOW_BARS {
            let (old_price, old_volume) = self.bars.pop_front().expect("non-empty window");
            self.weighted_price_sum -= i128::from(old_price) * i128::from(old_volume);
            self.volume_sum -= i128::from(old_volume);
        }

        if self.bars.len() != WINDOW_BARS || self.volume_sum <= 0 {
            return None;
        }

        Some(Price::from_raw((self.weighted_price_sum / self.volume_sum) as i64))
    }
}

struct MultiweekCostBasisReclaim {
    vwap: RollingVwap,
    previous_close: Option<Price>,
    previous_vwap: Option<Price>,
}

impl Strategy for MultiweekCostBasisReclaim {
    fn name(&self) -> String {
        "Multiweek Cost-Basis Reclaim".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        // The current bar is complete, so it may be included in the trailing VWAP.
        let current_vwap = self.vwap.push(bar);
        let (Some(current_vwap), Some(previous_close), Some(previous_vwap)) =
            (current_vwap, self.previous_close, self.previous_vwap)
        else {
            self.previous_close = Some(bar.close);
            self.previous_vwap = current_vwap;
            return;
        };

        let enter = previous_close < previous_vwap
            && bar.close >= current_vwap.mul_ratio(1_005, 1_000);
        let exit = previous_close > previous_vwap
            && bar.close <= current_vwap.mul_ratio(995, 1_000);

        self.previous_close = Some(bar.close);
        self.previous_vwap = Some(current_vwap);

        if ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if enter && position.is_zero() {
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                // Orders submitted after this close are intended for the next bar's open.
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        } else if exit && position.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
        }
    }
}

export_strategy!(MultiweekCostBasisReclaim {
    vwap: RollingVwap::new(),
    previous_close: None,
    previous_vwap: None,
});
