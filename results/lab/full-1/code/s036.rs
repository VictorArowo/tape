use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const ATR_DAYS: usize = 14;
const EMA_DAYS: usize = 10;
const SIZING_MARGIN_PPM: i64 = 5_000;

#[derive(Clone, Copy)]
struct DailyBar {
    day: i64,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    close_raw: i64,
}

struct DailyIndicators {
    previous_close: Option<f64>,
    atr_seed_sum: f64,
    atr_seed_count: usize,
    atr: Option<f64>,
    ema_seed_sum: f64,
    ema_seed_count: usize,
    ema: Option<f64>,
}

impl DailyIndicators {
    fn new() -> Self {
        Self {
            previous_close: None,
            atr_seed_sum: 0.0,
            atr_seed_count: 0,
            atr: None,
            ema_seed_sum: 0.0,
            ema_seed_count: 0,
            ema: None,
        }
    }

    fn update(&mut self, daily: &DailyBar) {
        let tr = if let Some(previous_close) = self.previous_close {
            (daily.high - daily.low)
                .max((daily.high - previous_close).abs())
                .max((daily.low - previous_close).abs())
        } else {
            daily.high - daily.low
        };

        if let Some(atr) = self.atr {
            self.atr = Some((atr * (ATR_DAYS as f64 - 1.0) + tr) / ATR_DAYS as f64);
        } else {
            self.atr_seed_sum += tr;
            self.atr_seed_count += 1;
            if self.atr_seed_count == ATR_DAYS {
                self.atr = Some(self.atr_seed_sum / ATR_DAYS as f64);
            }
        }
        self.previous_close = Some(daily.close);

        if let Some(ema) = self.ema {
            let alpha = 2.0 / (EMA_DAYS as f64 + 1.0);
            self.ema = Some(alpha * daily.close + (1.0 - alpha) * ema);
        } else {
            self.ema_seed_sum += daily.close;
            self.ema_seed_count += 1;
            if self.ema_seed_count == EMA_DAYS {
                self.ema = Some(self.ema_seed_sum / EMA_DAYS as f64);
            }
        }
    }
}

struct VolatilityScaledDrawdownGate {
    current_day: Option<DailyBar>,
    indicators: DailyIndicators,
    high_water: Option<f64>,
    last_exit_close: Option<f64>,
    pending_exit_close: Option<f64>,
    exit_pending: bool,
    entry_pending: bool,
}

impl VolatilityScaledDrawdownGate {
    fn new() -> Self {
        Self {
            current_day: None,
            indicators: DailyIndicators::new(),
            high_water: None,
            last_exit_close: None,
            pending_exit_close: None,
            exit_pending: false,
            entry_pending: false,
        }
    }

    fn add_minute(&mut self, bar: &Bar) -> Option<DailyBar> {
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let open_ns = bar.open_ts.as_nanos();
        let close_ns = bar.close_ts.as_nanos();
        let open_day = open_ns / day_ns;

        // Finalize an unfinished aggregate if the feed skips into another UTC day.
        let mut skipped_day = None;
        if self.current_day.is_some_and(|daily| daily.day != open_day) {
            skipped_day = self.current_day.take();
        }

        if self.current_day.is_none() {
            self.current_day = Some(DailyBar {
                day: open_day,
                open: bar.open.to_f64(),
                high: bar.high.to_f64(),
                low: bar.low.to_f64(),
                close: bar.close.to_f64(),
                close_raw: bar.close.raw(),
            });
        } else if let Some(daily) = self.current_day.as_mut() {
            daily.high = daily.high.max(bar.high.to_f64());
            daily.low = daily.low.min(bar.low.to_f64());
            daily.close = bar.close.to_f64();
            daily.close_raw = bar.close.raw();
        }

        let open_minute = (open_ns / minute_ns) % 1440;
        let close_day = close_ns / day_ns;
        let close_minute = (close_ns / minute_ns) % 1440;
        let day_finished = close_day != open_day || open_minute == 1439 || close_minute == 1439;
        let completed = if day_finished {
            self.current_day.take()
        } else {
            None
        };

        // A skipped aggregate is processed before the current day's completed bar.
        if let Some(skipped) = skipped_day {
            self.process_daily(&skipped);
        }
        completed
    }

    fn process_daily(&mut self, daily: &DailyBar) {
        self.indicators.update(daily);
        let atr = self.indicators.atr;
        let ema = self.indicators.ema;

        // Daily closes, rather than minute highs or lows, define the position high-water mark.
        // The position is checked by the caller before this method is invoked.
        if let Some(high_water) = self.high_water.as_mut() {
            *high_water = high_water.max(daily.close);
        }

        // Trading decisions are applied separately, after indicator updates, in on_bar.
        let _ = (atr, ema);
    }

    fn evaluate_daily_signal(&mut self, ctx: &mut Ctx, daily: &DailyBar) {
        let position = ctx.position(INSTRUMENT);
        let close = daily.close;
        let atr = self.indicators.atr;

        if position.is_positive() {
            let high_water = self.high_water.get_or_insert(close);
            *high_water = high_water.max(close);

            if !self.exit_pending {
                if let Some(atr) = atr {
                    if *high_water - close >= 1.25 * atr {
                        self.exit_pending = true;
                        self.pending_exit_close = Some(close);
                        self.entry_pending = false;
                    }
                }
            }
            return;
        }

        if self.exit_pending || self.entry_pending {
            return;
        }

        if let (Some(exit_close), Some(atr), Some(ema)) =
            (self.last_exit_close, atr, self.indicators.ema)
        {
            if close - exit_close >= 0.5 * atr && close > ema {
                self.entry_pending = true;
            }
        }
    }

    fn service_orders(&mut self, ctx: &mut Ctx, reference_price: Price) {
        if ctx.open_orders() != 0 {
            return;
        }

        if self.exit_pending {
            let position = ctx.position(INSTRUMENT);
            if position.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
            } else {
                self.exit_pending = false;
                self.high_water = None;
                self.last_exit_close = self.pending_exit_close.take();
            }
            return;
        }

        if self.entry_pending {
            if !reference_price.is_positive() {
                return;
            }
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                reference_price,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            } else {
                self.entry_pending = false;
            }
        }
    }
}

impl Strategy for VolatilityScaledDrawdownGate {
    fn name(&self) -> String {
        "volatility-scaled-drawdown-gate".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let completed = self.add_minute(bar);
        if let Some(daily) = completed {
            self.process_daily(&daily);
            self.evaluate_daily_signal(ctx, &daily);
        }

        // Retry IOC orders as needed, including partial fills limited by execution volume.
        self.service_orders(ctx, bar.close);
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        if fill.order_id == fill.order_id {
            match fill.side {
                Side::Buy => {
                    self.high_water = None;
                    self.last_exit_close = None;
                }
                Side::Sell => {
                    if !ctx.position(INSTRUMENT).is_positive() {
                        self.exit_pending = false;
                        self.high_water = None;
                        self.entry_pending = false;
                        self.last_exit_close = self.pending_exit_close.take();
                    }
                }
            }
        }
    }
}

export_strategy!(VolatilityScaledDrawdownGate::new());