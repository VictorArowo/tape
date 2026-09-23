use tape_guest::{Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;

struct SessionVwapReclaim {
    day: Option<i64>,
    cumulative_pv: f64,
    cumulative_volume: f64,
    previous_close: Option<f64>,
    previous_vwap: Option<f64>,
}

impl SessionVwapReclaim {
    fn new() -> Self {
        Self {
            day: None,
            cumulative_pv: 0.0,
            cumulative_volume: 0.0,
            previous_close: None,
            previous_vwap: None,
        }
    }
}

impl Strategy for SessionVwapReclaim {
    fn name(&self) -> String {
        "Session VWAP Reclaim".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let day = bar.close_ts.as_nanos() / TimeDelta::from_days(1).as_nanos();
        let new_day = self.day != Some(day);
        if new_day {
            self.day = Some(day);
            self.cumulative_pv = 0.0;
            self.cumulative_volume = 0.0;
            self.previous_close = None;
            self.previous_vwap = None;
        }

        let typical_price = (bar.high.to_f64() + bar.low.to_f64() + bar.close.to_f64()) / 3.0;
        let volume = bar.volume.to_f64();
        self.cumulative_pv += typical_price * volume;
        self.cumulative_volume += volume;

        let current_vwap = if self.cumulative_volume > 0.0 {
            Some(self.cumulative_pv / self.cumulative_volume)
        } else {
            None
        };

        // The first completed bar of each UTC day initializes the session and
        // cannot generate a trading signal.
        if !new_day && ctx.open_orders() == 0 {
            if let (Some(previous_close), Some(previous_vwap), Some(current_vwap)) =
                (self.previous_close, self.previous_vwap, current_vwap)
            {
                let position = ctx.position(INSTRUMENT);
                if position.is_zero()
                    && previous_close < previous_vwap
                    && bar.close.to_f64() > current_vwap
                {
                    let qty = ctx.affordable_qty(
                        INSTRUMENT,
                        bar.close,
                        ctx.cash(),
                        SIZING_MARGIN_PPM,
                    );
                    if qty.is_positive() {
                        ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                    }
                } else if position.is_positive() && bar.close.to_f64() < current_vwap {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                }
            }
        }

        self.previous_close = Some(bar.close.to_f64());
        self.previous_vwap = current_vwap;
    }
}

export_strategy!(SessionVwapReclaim::new());
