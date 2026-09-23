use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const WINDOW_MINUTES: usize = 30;
const LOOKBACK_DAYS: usize = 28;
const NO_LEVERAGE_MARGIN_PPM: i64 = 1_000_000;

struct MinuteSample {
    open_raw: i64,
    close_ns: i64,
    volume_raw: i64,
}

struct VolumeSample {
    day: i64,
    volume_raw: i128,
}

struct FundingClockContinuation {
    minutes: VecDeque<MinuteSample>,
    volumes: [VecDeque<VolumeSample>; 3],
    entry_ts_ns: Option<i64>,
}

impl FundingClockContinuation {
    fn new() -> Self {
        Self {
            minutes: VecDeque::with_capacity(WINDOW_MINUTES),
            volumes: std::array::from_fn(|_| VecDeque::with_capacity(LOOKBACK_DAYS)),
            entry_ts_ns: None,
        }
    }

    fn boundary_slot(close_ns: i64, minute_ns: i64, day_ns: i64) -> Option<(usize, i64)> {
        let day = close_ns.div_euclid(day_ns);
        let minute_of_day = close_ns.div_euclid(minute_ns).rem_euclid(1440);
        let slot = match minute_of_day {
            0 => 0,
            480 => 1,
            960 => 2,
            _ => return None,
        };
        Some((slot, day))
    }

    fn completed_window(
        minutes: &VecDeque<MinuteSample>,
        close_ns: i64,
    ) -> Option<(i64, i128)> {
        if minutes.len() != WINDOW_MINUTES || minutes.back()?.close_ns != close_ns {
            return None;
        }

        let first_open_raw = minutes.front()?.open_raw;
        let volume_raw = minutes
            .iter()
            .map(|sample| i128::from(sample.volume_raw))
            .sum();
        Some((first_open_raw, volume_raw))
    }
}

impl Strategy for FundingClockContinuation {
    fn name(&self) -> String {
        "Funding-Clock Continuation".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let open_ns = bar.open_ts.as_nanos();
        let close_ns = bar.close_ts.as_nanos();

        if close_ns - open_ns != minute_ns
            || self
                .minutes
                .back()
                .is_some_and(|previous| previous.close_ns != open_ns)
        {
            self.minutes.clear();
        }

        self.minutes.push_back(MinuteSample {
            open_raw: bar.open.raw(),
            close_ns,
            volume_raw: bar.volume.raw(),
        });
        if self.minutes.len() > WINDOW_MINUTES {
            self.minutes.pop_front();
        }

        let mut entry_signal = false;
        if let Some((slot, day)) = Self::boundary_slot(close_ns, minute_ns, day_ns) {
            // Compute this before mutably borrowing the history for this slot.
            let completed = Self::completed_window(&self.minutes, close_ns);
            let first_day = day.saturating_sub(LOOKBACK_DAYS as i64);
            let history = &mut self.volumes[slot];
            while history.front().is_some_and(|sample| sample.day < first_day) {
                history.pop_front();
            }

            if let Some((first_open_raw, current_volume)) = completed {
                if history.len() == LOOKBACK_DAYS {
                    let mut sorted_volumes: Vec<i128> =
                        history.iter().map(|sample| sample.volume_raw).collect();
                    sorted_volumes.sort_unstable();
                    let middle_sum = sorted_volumes[13] + sorted_volumes[14];
                    let volume_is_high = current_volume * 4 >= middle_sum * 3;
                    let return_is_high = first_open_raw > 0
                        && i128::from(bar.close.raw()) * 1000
                            >= i128::from(first_open_raw) * 1003;
                    entry_signal = volume_is_high && return_is_high;
                }

                history.push_back(VolumeSample {
                    day,
                    volume_raw: current_volume,
                });
                while history.len() > LOOKBACK_DAYS {
                    history.pop_front();
                }
            }
        }

        if ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        if position.is_positive() {
            if let Some(entry_ns) = self.entry_ts_ns {
                let hold_ns = TimeDelta::from_mins(120).as_nanos();
                if close_ns >= entry_ns.saturating_add(hold_ns) {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                }
            }
            return;
        }

        if !position.is_zero() {
            return;
        }
        self.entry_ts_ns = None;

        if entry_signal {
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                bar.close,
                ctx.cash(),
                NO_LEVERAGE_MARGIN_PPM,
            );
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        }
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &tape_guest::Fill) {
        match fill.side {
            Side::Buy => {
                if self.entry_ts_ns.is_none() {
                    self.entry_ts_ns = Some(fill.ts.as_nanos());
                }
            }
            Side::Sell => {
                if ctx.position(INSTRUMENT).is_zero() {
                    self.entry_ts_ns = None;
                }
            }
        }
    }
}

export_strategy!(FundingClockContinuation::new());
