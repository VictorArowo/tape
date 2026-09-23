use tape_guest::{
    Bar, Ctx, InstrumentId, Money, OrderRequest, Qty, Side, Strategy, TimeDelta, TimeInForce,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const VOLUME_PARTICIPATION_NUM: i64 = 10;
const VOLUME_PARTICIPATION_DEN: i64 = 100;

#[derive(Clone, Copy, PartialEq, Eq)]
enum OrderRole {
    Core,
    TacticalEntry,
    TacticalExit,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Campaign {
    Idle,
    Entering,
    Exiting,
}

struct PaydayDemandSleeve {
    core_target: Option<Qty>,
    core_filled: Qty,
    core_budget: Money,
    core_spent: Money,
    tactical_target: Option<Qty>,
    tactical_filled: Qty,
    campaign: Campaign,
    submitted_role: Option<OrderRole>,
}

fn zero_qty() -> Qty {
    Qty::from_raw(0)
}

fn zero_money() -> Money {
    Money::from_raw(0)
}

fn min_qty(a: Qty, b: Qty) -> Qty {
    if a < b { a } else { b }
}

fn min_money(a: Money, b: Money) -> Money {
    if a < b { a } else { b }
}

fn positive_money(value: Money) -> Money {
    if value.is_positive() { value } else { zero_money() }
}

fn volume_slice(volume: Qty) -> Qty {
    volume.mul_ratio(VOLUME_PARTICIPATION_NUM, VOLUME_PARTICIPATION_DEN)
}

// Returns the UTC month, day of month, and minute of day for a timestamp.
fn utc_calendar(timestamp_nanos: i64) -> (i64, i64, i64) {
    let day_ns = TimeDelta::from_days(1).as_nanos();
    let minute_ns = TimeDelta::from_mins(1).as_nanos();
    let epoch_days = timestamp_nanos.div_euclid(day_ns);
    let minute_of_day = timestamp_nanos.rem_euclid(day_ns) / minute_ns;

    // Convert days since the Unix epoch to a Gregorian calendar date.
    let z = epoch_days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524
        - day_of_era / 146_096)
        / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era
        - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    let _year = year + if month <= 2 { 1 } else { 0 };

    (month, day, minute_of_day)
}

impl PaydayDemandSleeve {
    fn submit_market(&mut self, ctx: &mut Ctx, role: OrderRole, side: Side, qty: Qty) {
        if !qty.is_positive() || ctx.open_orders() != 0 {
            return;
        }
        self.submitted_role = Some(role);
        ctx.submit(
            OrderRequest::market(INSTRUMENT, side, qty)
                .with_time_in_force(TimeInForce::Ioc),
        );
    }

    fn initialize_core(&mut self, ctx: &Ctx, bar: &Bar) {
        if self.core_target.is_some() {
            return;
        }
        let budget = positive_money(ctx.cash().mul_ratio(80, 100));
        let target = if budget.is_positive() {
            ctx.affordable_qty(INSTRUMENT, bar.close, budget, SIZING_MARGIN_PPM)
        } else {
            zero_qty()
        };
        self.core_budget = budget;
        self.core_target = Some(target);
    }

    fn begin_tactical_entry(&mut self, ctx: &Ctx, bar: &Bar) {
        let equity_budget = positive_money(ctx.equity().mul_ratio(20, 100));
        let position_value = bar.close * ctx.position(INSTRUMENT).abs();
        let exposure_headroom = positive_money(ctx.equity() - position_value);
        let budget = min_money(
            min_money(equity_budget, positive_money(ctx.cash())),
            exposure_headroom,
        );
        let target = if budget.is_positive() {
            ctx.affordable_qty(INSTRUMENT, bar.close, budget, SIZING_MARGIN_PPM)
        } else {
            zero_qty()
        };
        self.tactical_target = Some(target);
        self.campaign = Campaign::Entering;
    }
}

impl Strategy for PaydayDemandSleeve {
    fn name(&self) -> String {
        "payday-demand-sleeve [wasm]".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        self.initialize_core(ctx, bar);

        let (_, day, minute_of_day) = utc_calendar(bar.open_ts.as_nanos());
        let at_midnight = minute_of_day == 0;

        if self.campaign == Campaign::Exiting && self.tactical_filled.is_zero() {
            self.campaign = Campaign::Idle;
            self.tactical_target = None;
        }

        if at_midnight && (day == 3 || day == 17) {
            self.campaign = Campaign::Exiting;
            self.tactical_target = None;
        } else if at_midnight
            && (day == 1 || day == 15)
            && self.campaign == Campaign::Idle
            && self.tactical_filled.is_zero()
        {
            self.begin_tactical_entry(ctx, bar);
        }

        if ctx.open_orders() != 0 {
            return;
        }

        let max_this_slice = volume_slice(bar.volume);
        if !max_this_slice.is_positive() {
            return;
        }

        // Scheduled tactical exits take priority over any unfinished core entry.
        if self.campaign == Campaign::Exiting && self.tactical_filled.is_positive() {
            let qty = min_qty(self.tactical_filled, max_this_slice);
            self.submit_market(ctx, OrderRole::TacticalExit, Side::Sell, qty);
            return;
        }

        // Complete the initial 80% core allocation, using only the reserved budget.
        if let Some(target) = self.core_target {
            if self.core_filled < target {
                let remaining = target - self.core_filled;
                let budget_left = positive_money(self.core_budget - self.core_spent);
                let available_budget = min_money(budget_left, positive_money(ctx.cash()));
                let affordable = if available_budget.is_positive() {
                    ctx.affordable_qty(
                        INSTRUMENT,
                        bar.close,
                        available_budget,
                        SIZING_MARGIN_PPM,
                    )
                } else {
                    zero_qty()
                };
                let qty = min_qty(min_qty(remaining, affordable), max_this_slice);
                if qty.is_positive() {
                    self.submit_market(ctx, OrderRole::Core, Side::Buy, qty);
                    return;
                }
            }
        }

        // Retry any unfilled tactical entry quantity on subsequent bars until its exit date.
        if self.campaign == Campaign::Entering {
            if let Some(target) = self.tactical_target {
                if self.tactical_filled < target {
                    let remaining = target - self.tactical_filled;
                    let position_value = bar.close * ctx.position(INSTRUMENT).abs();
                    let headroom = positive_money(ctx.equity() - position_value);
                    let budget = min_money(positive_money(ctx.cash()), headroom);
                    let affordable = if budget.is_positive() {
                        ctx.affordable_qty(INSTRUMENT, bar.close, budget, SIZING_MARGIN_PPM)
                    } else {
                        zero_qty()
                    };
                    let qty = min_qty(min_qty(remaining, affordable), max_this_slice);
                    self.submit_market(ctx, OrderRole::TacticalEntry, Side::Buy, qty);
                }
            }
        }
    }

    fn on_fill(&mut self, _ctx: &mut Ctx, fill: &tape_guest::Fill) {
        let Some(role) = self.submitted_role else {
            return;
        };

        match (role, fill.side) {
            (OrderRole::Core, Side::Buy) => {
                self.core_filled = self.core_filled + fill.qty;
                self.core_spent = self.core_spent + fill.price * fill.qty + fill.fee;
            }
            (OrderRole::TacticalEntry, Side::Buy) => {
                self.tactical_filled = self.tactical_filled + fill.qty;
            }
            (OrderRole::TacticalExit, Side::Sell) => {
                self.tactical_filled = if fill.qty >= self.tactical_filled {
                    zero_qty()
                } else {
                    self.tactical_filled - fill.qty
                };
            }
            _ => {}
        }
    }
}

export_strategy!(PaydayDemandSleeve {
    core_target: None,
    core_filled: Qty::from_raw(0),
    core_budget: Money::from_raw(0),
    core_spent: Money::from_raw(0),
    tactical_target: None,
    tactical_filled: Qty::from_raw(0),
    campaign: Campaign::Idle,
    submitted_role: None,
});
