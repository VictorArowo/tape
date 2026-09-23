//! The event loop and the simulated exchange.
//!
//! Two queues drive a run: market events from the data, in time order, and
//! commands from the strategy (submits and cancels), each scheduled to
//! arrive after the configured latency. The loop always takes whichever is
//! earlier. When they share a timestamp the market event goes first, so an
//! order arriving at time `t` can never match a trade that printed at `t`.
//!
//! Order fills follow these rules, from the moment an order arrives:
//!
//! - **Market orders** fill at the next event's price: a bar's open or the
//!   next trade, moved against the order by the slippage setting.
//! - **Limit orders** that cross the last price on arrival behave like
//!   market orders capped at their limit. Otherwise they rest, and fill at
//!   their limit price as a maker once the market trades through it (or
//!   touches it, if the model allows that).
//! - **Stop orders** trigger when the market reaches the trigger price and
//!   then fill like market orders. If a bar opens beyond the trigger, the
//!   fill is at the open, not the trigger: gaps hurt.
//! - Each bar's fills are capped at a share of its volume, and a resting
//!   order can't fill more than the trade that crossed it.
//! - A fill that would overdraw cash, or sell more than is held on a
//!   spot account, cancels the order instead.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};
use std::error::Error;

use thiserror::Error;

use tape_core::{
    Bar, Fill, Instrument, InstrumentId, Liquidity, MarketEvent, Money, OrderId, OrderRequest,
    OrderType, Price, Qty, Side, TimeInForce, Timestamp, Trade,
};

use crate::config::{BacktestConfig, PPM};
use crate::portfolio::Portfolio;
use crate::report::{BacktestResult, EquityPoint, OrderRecord, OrderStatus, fingerprint};
use crate::strategy::{
    CancelReason, Command, Context, OpenOrder, OrderState, OrderUpdate, RejectReason, Strategy,
};

/// A strategy that keeps sending commands at one instant without time
/// moving forward (for example resubmitting every rejected order with zero
/// latency) would otherwise loop forever.
const MAX_COMMANDS_PER_INSTANT: u32 = 100_000;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("market data error: {0}")]
    Data(Box<dyn Error + Send + Sync>),
    #[error("market data goes backwards: {previous} then {next}")]
    OutOfOrder {
        previous: Timestamp,
        next: Timestamp,
    },
    #[error("market data for unknown instrument {0}")]
    UnknownInstrument(InstrumentId),
    #[error(
        "strategy sent more than {MAX_COMMANDS_PER_INSTANT} commands at {0} without time advancing"
    )]
    RunawayStrategy(Timestamp),
}

pub struct Backtest {
    config: BacktestConfig,
    instruments: BTreeMap<InstrumentId, Instrument>,
}

impl Backtest {
    pub fn new(config: BacktestConfig, instruments: impl IntoIterator<Item = Instrument>) -> Self {
        let instruments = instruments.into_iter().map(|i| (i.id, i)).collect();
        Self {
            config,
            instruments,
        }
    }

    pub fn config(&self) -> &BacktestConfig {
        &self.config
    }

    pub fn run<E>(
        &self,
        strategy: &mut dyn Strategy,
        events: impl IntoIterator<Item = Result<MarketEvent, E>>,
    ) -> Result<BacktestResult, EngineError>
    where
        E: Into<Box<dyn Error + Send + Sync>>,
    {
        let mut run = Run::new(&self.config, &self.instruments);
        let mut events = events.into_iter();
        let mut pull = |run: &Run<'_>| -> Result<Option<MarketEvent>, EngineError> {
            match events.next() {
                None => Ok(None),
                Some(Err(err)) => Err(EngineError::Data(err.into())),
                Some(Ok(event)) => {
                    if !run.instruments.contains_key(&event.instrument()) {
                        return Err(EngineError::UnknownInstrument(event.instrument()));
                    }
                    if let Some((_, last)) = run.span
                        && event.ts() < last
                    {
                        return Err(EngineError::OutOfOrder {
                            previous: last,
                            next: event.ts(),
                        });
                    }
                    Ok(Some(event))
                }
            }
        };

        let mut next = pull(&run)?;
        run.now = next.as_ref().map_or(Timestamp::EPOCH, MarketEvent::ts);
        run.callback(strategy, |s, ctx| s.on_start(ctx));

        let mut instant = (run.now, 0_u32);
        while let Some(event) = next.take() {
            let command_due = run.schedule.peek().map(|Reverse((ts, _))| *ts);
            match command_due {
                Some(due) if due < event.ts() => {
                    next = Some(event);
                    if instant.0 == due {
                        instant.1 += 1;
                        if instant.1 > MAX_COMMANDS_PER_INSTANT {
                            return Err(EngineError::RunawayStrategy(due));
                        }
                    } else {
                        instant = (due, 1);
                    }
                    run.process_command(strategy);
                }
                _ => {
                    run.process_market(strategy, &event);
                    next = pull(&run)?;
                }
            }
        }

        run.finish(strategy);
        Ok(run.into_result(strategy.name()))
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Mode {
    /// Fills at the next event's price.
    Taker,
    /// A limit order waiting for the market to come to it.
    Resting,
    /// A stop order waiting for its trigger.
    StopPending,
}

struct Working {
    request: OrderRequest,
    remaining: Qty,
    mode: Mode,
}

enum Notice {
    Fill(Fill),
    Order(OrderUpdate),
}

/// Remaining capacity for fills within one market event.
struct Capacity {
    /// Bar volume still available under the participation cap.
    bar_volume: Option<Qty>,
    /// Trade quantity still available to resting orders.
    trade_qty: Option<Qty>,
}

struct Run<'a> {
    config: &'a BacktestConfig,
    instruments: &'a BTreeMap<InstrumentId, Instrument>,
    now: Timestamp,
    portfolio: Portfolio,
    marks: BTreeMap<InstrumentId, Price>,
    open_orders: BTreeMap<OrderId, OpenOrder>,
    working: BTreeMap<OrderId, Working>,
    records: BTreeMap<OrderId, OrderRecord>,
    fills: Vec<Fill>,
    equity: Vec<EquityPoint>,
    next_sample: Option<Timestamp>,
    schedule: BinaryHeap<Reverse<(Timestamp, u64)>>,
    scheduled: BTreeMap<u64, Command>,
    seq: u64,
    next_order_id: u64,
    outbox: Vec<Command>,
    notices: Vec<Notice>,
    events: u64,
    span: Option<(Timestamp, Timestamp)>,
}

impl<'a> Run<'a> {
    fn new(
        config: &'a BacktestConfig,
        instruments: &'a BTreeMap<InstrumentId, Instrument>,
    ) -> Self {
        Self {
            config,
            instruments,
            now: Timestamp::EPOCH,
            portfolio: Portfolio::new(config.initial_cash),
            marks: BTreeMap::new(),
            open_orders: BTreeMap::new(),
            working: BTreeMap::new(),
            records: BTreeMap::new(),
            fills: Vec::new(),
            equity: Vec::new(),
            next_sample: None,
            schedule: BinaryHeap::new(),
            scheduled: BTreeMap::new(),
            seq: 0,
            next_order_id: 0,
            outbox: Vec::new(),
            notices: Vec::new(),
            events: 0,
            span: None,
        }
    }

    /// Runs a strategy callback, then schedules whatever it sent.
    fn callback(
        &mut self,
        strategy: &mut dyn Strategy,
        f: impl FnOnce(&mut dyn Strategy, &mut Context<'_>),
    ) {
        let mut ctx = Context {
            now: self.now,
            portfolio: &self.portfolio,
            marks: &self.marks,
            instruments: self.instruments,
            open_orders: &self.open_orders,
            fees: &self.config.execution.fees,
            next_order_id: &mut self.next_order_id,
            outbox: &mut self.outbox,
        };
        f(strategy, &mut ctx);
        let arrival = self.now + self.config.execution.latency;
        for command in std::mem::take(&mut self.outbox) {
            if let Command::Submit(id, request) = &command {
                self.open_orders.insert(
                    *id,
                    OpenOrder {
                        request: request.clone(),
                        remaining: request.qty,
                        state: OrderState::InFlight,
                    },
                );
                self.records.insert(
                    *id,
                    OrderRecord {
                        id: *id,
                        request: request.clone(),
                        submitted_at: self.now,
                        arrived_at: None,
                        finished_at: None,
                        filled: Qty::ZERO,
                        status: OrderStatus::InFlight,
                    },
                );
            }
            self.seq += 1;
            self.schedule.push(Reverse((arrival, self.seq)));
            self.scheduled.insert(self.seq, command);
        }
    }

    /// Delivers queued fills and order updates to the strategy. Callbacks
    /// can queue more, so this drains until nothing is left.
    fn deliver(&mut self, strategy: &mut dyn Strategy) {
        while !self.notices.is_empty() {
            for notice in std::mem::take(&mut self.notices) {
                match notice {
                    Notice::Fill(fill) => self.callback(strategy, |s, ctx| s.on_fill(ctx, &fill)),
                    Notice::Order(update) => {
                        self.callback(strategy, |s, ctx| s.on_order(ctx, &update));
                    }
                }
            }
        }
    }

    fn finish_order(&mut self, id: OrderId, status: OrderStatus) {
        self.working.remove(&id);
        self.open_orders.remove(&id);
        if let Some(record) = self.records.get_mut(&id) {
            record.status = status;
            record.finished_at = Some(self.now);
        }
    }

    fn cancel(&mut self, id: OrderId, reason: CancelReason) {
        let remaining = self
            .open_orders
            .get(&id)
            .map_or(Qty::ZERO, |order| order.remaining);
        self.finish_order(id, OrderStatus::Cancelled(reason));
        self.notices.push(Notice::Order(OrderUpdate::Cancelled {
            id,
            ts: self.now,
            remaining,
            reason,
        }));
    }

    fn reject(&mut self, id: OrderId, reason: RejectReason) {
        self.finish_order(id, OrderStatus::Rejected(reason));
        self.notices.push(Notice::Order(OrderUpdate::Rejected {
            id,
            ts: self.now,
            reason,
        }));
    }

    fn process_command(&mut self, strategy: &mut dyn Strategy) {
        let Reverse((ts, seq)) = self.schedule.pop().expect("caller checked the schedule");
        self.now = ts;
        match self
            .scheduled
            .remove(&seq)
            .expect("scheduled command exists")
        {
            Command::Submit(id, request) => self.arrive(id, request),
            Command::Cancel(id) => {
                if self.working.contains_key(&id) {
                    self.cancel(id, CancelReason::Requested);
                }
            }
        }
        self.deliver(strategy);
    }

    /// Price used to estimate what an order will cost.
    fn reference_price(&self, request: &OrderRequest) -> Option<Price> {
        match request.order_type {
            OrderType::Limit { price } => Some(price),
            OrderType::Stop { trigger } => Some(trigger),
            OrderType::Market => self.marks.get(&request.instrument).copied(),
        }
    }

    fn validate(&self, id: OrderId, request: &OrderRequest) -> Result<Mode, RejectReason> {
        let instrument = self
            .instruments
            .get(&request.instrument)
            .ok_or(RejectReason::UnknownInstrument)?;
        if !instrument.is_valid_qty(request.qty) {
            return Err(RejectReason::InvalidQty);
        }
        if let OrderType::Limit { price } | OrderType::Stop { trigger: price } = request.order_type
            && !instrument.is_valid_price(price)
        {
            return Err(RejectReason::InvalidPrice);
        }

        let others = || {
            self.working
                .iter()
                .filter(move |(other, w)| {
                    **other != id && w.request.instrument == request.instrument
                })
                .map(|(_, w)| w)
        };
        match request.side {
            Side::Sell if !self.config.allow_short => {
                let committed: Qty = others()
                    .filter(|w| w.request.side == Side::Sell)
                    .map(|w| w.remaining)
                    .sum();
                if request.qty > self.portfolio.position(request.instrument) - committed {
                    return Err(RejectReason::InsufficientPosition);
                }
            }
            Side::Buy => {
                let fees = &self.config.execution.fees;
                let estimate = |request: &OrderRequest, qty: Qty| {
                    self.reference_price(request).map_or(Money::ZERO, |price| {
                        (price * qty).mul_ratio(PPM + fees.taker_ppm, PPM)
                    })
                };
                let reserved: Money = others()
                    .filter(|w| w.request.side == Side::Buy)
                    .map(|w| estimate(&w.request, w.remaining))
                    .sum();
                if estimate(request, request.qty) > self.portfolio.cash() - reserved {
                    return Err(RejectReason::InsufficientCash);
                }
            }
            Side::Sell => {}
        }

        Ok(match request.order_type {
            OrderType::Market => Mode::Taker,
            OrderType::Stop { .. } => Mode::StopPending,
            OrderType::Limit { price } => {
                let marketable =
                    self.marks
                        .get(&request.instrument)
                        .is_some_and(|mark| match request.side {
                            Side::Buy => price >= *mark,
                            Side::Sell => price <= *mark,
                        });
                if marketable {
                    Mode::Taker
                } else {
                    Mode::Resting
                }
            }
        })
    }

    fn arrive(&mut self, id: OrderId, request: OrderRequest) {
        match self.validate(id, &request) {
            Err(reason) => self.reject(id, reason),
            Ok(mode) => {
                if let Some(record) = self.records.get_mut(&id) {
                    record.arrived_at = Some(self.now);
                    record.status = OrderStatus::Working;
                }
                if let Some(open) = self.open_orders.get_mut(&id) {
                    open.state = OrderState::Working;
                }
                self.working.insert(
                    id,
                    Working {
                        remaining: request.qty,
                        request,
                        mode,
                    },
                );
                self.notices
                    .push(Notice::Order(OrderUpdate::Accepted { id, ts: self.now }));
            }
        }
    }

    fn process_market(&mut self, strategy: &mut dyn Strategy, event: &MarketEvent) {
        self.now = event.ts();
        self.events += 1;
        self.span = Some(match self.span {
            None => (self.now, self.now),
            Some((first, _)) => (first, self.now),
        });

        let instrument_id = event.instrument();
        let instrument = &self.instruments[&instrument_id];
        let execution = &self.config.execution;
        let mut capacity = match event {
            MarketEvent::Bar(bar) => Capacity {
                bar_volume: execution
                    .max_bar_participation_ppm
                    .map(|ppm| round_down(bar.volume.mul_ratio(ppm, PPM), instrument.lot_size)),
                trade_qty: None,
            },
            MarketEvent::Trade(trade) => Capacity {
                bar_volume: None,
                trade_qty: Some(trade.qty),
            },
        };

        let ids: Vec<OrderId> = self
            .working
            .iter()
            .filter(|(_, w)| w.request.instrument == instrument_id)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.match_order(id, event, &mut capacity);
            if let Some(w) = self.working.get(&id)
                && w.request.time_in_force == TimeInForce::Ioc
            {
                self.cancel(id, CancelReason::ImmediateOrCancel);
            }
        }

        let mark = match event {
            MarketEvent::Bar(bar) => bar.close,
            MarketEvent::Trade(trade) => trade.price,
        };
        self.marks.insert(instrument_id, mark);
        self.sample_equity(false);

        self.deliver(strategy);
        self.callback(strategy, |s, ctx| s.on_market(ctx, event));
    }

    fn sample_equity(&mut self, force: bool) {
        let interval = self.config.equity_interval.as_nanos().max(1);
        let due = *self.next_sample.get_or_insert(Timestamp::from_nanos(
            self.now.as_nanos().div_euclid(interval) * interval,
        ));
        let last_ts = self.equity.last().map(|p| p.ts);
        if (self.now >= due || force) && last_ts != Some(self.now) {
            self.equity.push(EquityPoint {
                ts: self.now,
                equity: self.portfolio.equity(&self.marks),
            });
            let next = (self.now.as_nanos().div_euclid(interval) + 1) * interval;
            self.next_sample = Some(Timestamp::from_nanos(next));
        }
    }

    fn slipped(&self, price: Price, side: Side, tick: Price) -> Price {
        let ppm = self.config.execution.slippage_ppm;
        if ppm == 0 {
            return price;
        }
        let factor = match side {
            Side::Buy => PPM + ppm,
            Side::Sell => PPM - ppm,
        };
        let moved = round_to_tick(price.mul_ratio(factor, PPM), tick, side == Side::Buy);
        moved.max(tick)
    }

    /// Where, if anywhere, an order fills against one event.
    fn fill_price(
        &mut self,
        id: OrderId,
        event: &MarketEvent,
        tick: Price,
    ) -> Option<(Price, Liquidity)> {
        let through_only = self.config.execution.require_trade_through;
        let w = &self.working[&id];
        let (side, mode, order_type) = (w.request.side, w.mode, w.request.order_type);
        let buy = side == Side::Buy;

        // The price a taker order would get, capped at its limit if it has one.
        let taker = |run: &Self, base: Price| -> Option<Price> {
            let price = run.slipped(base, side, tick);
            match order_type {
                OrderType::Limit { price: limit } => {
                    let within = |p: Price| if buy { p <= limit } else { p >= limit };
                    if within(price) {
                        Some(price)
                    } else if within(base) {
                        Some(limit)
                    } else {
                        None
                    }
                }
                _ => Some(price),
            }
        };
        let resting = |limit: Price| -> Option<Price> {
            let crossed = match event {
                MarketEvent::Bar(Bar { low, high, .. }) => {
                    if buy {
                        *low < limit || (!through_only && *low == limit)
                    } else {
                        *high > limit || (!through_only && *high == limit)
                    }
                }
                MarketEvent::Trade(Trade {
                    price, aggressor, ..
                }) => {
                    let through = if buy { *price < limit } else { *price > limit };
                    let touched = *price == limit && *aggressor == side.opposite();
                    through || (!through_only && touched)
                }
            };
            crossed.then_some(limit)
        };
        let open = match event {
            MarketEvent::Bar(bar) => bar.open,
            MarketEvent::Trade(trade) => trade.price,
        };

        match mode {
            Mode::Taker => {
                if let Some(price) = taker(self, open) {
                    return Some((price, Liquidity::Taker));
                }
                // The market moved away from a marketable limit before it
                // arrived; from here on it rests in the book.
                let OrderType::Limit { price: limit } = order_type else {
                    unreachable!("only limit orders can fail to fill as takers")
                };
                self.working.get_mut(&id).expect("working").mode = Mode::Resting;
                resting(limit).map(|price| (price, Liquidity::Maker))
            }
            Mode::Resting => {
                let OrderType::Limit { price: limit } = order_type else {
                    unreachable!("only limit orders rest")
                };
                resting(limit).map(|price| (price, Liquidity::Maker))
            }
            Mode::StopPending => {
                let OrderType::Stop { trigger } = order_type else {
                    unreachable!("only stop orders wait for a trigger")
                };
                let (triggered, base) = match event {
                    MarketEvent::Bar(bar) if buy => (bar.high >= trigger, bar.open.max(trigger)),
                    MarketEvent::Bar(bar) => (bar.low <= trigger, bar.open.min(trigger)),
                    MarketEvent::Trade(trade) if buy => (trade.price >= trigger, trade.price),
                    MarketEvent::Trade(trade) => (trade.price <= trigger, trade.price),
                };
                if !triggered {
                    return None;
                }
                self.working.get_mut(&id).expect("working").mode = Mode::Taker;
                Some((self.slipped(base, side, tick), Liquidity::Taker))
            }
        }
    }

    fn match_order(&mut self, id: OrderId, event: &MarketEvent, capacity: &mut Capacity) {
        let instrument = &self.instruments[&event.instrument()];
        let (tick, lot) = (instrument.tick_size, instrument.lot_size);
        let Some((price, liquidity)) = self.fill_price(id, event, tick) else {
            return;
        };

        let w = &self.working[&id];
        let side = w.request.side;
        let mut qty = w.remaining;
        if let Some(volume) = capacity.bar_volume {
            qty = qty.min(volume);
        }
        if liquidity == Liquidity::Maker
            && let Some(available) = capacity.trade_qty
        {
            qty = qty.min(available);
        }
        let qty = round_down(qty, lot);
        if qty.is_zero() {
            return;
        }

        let notional = price * qty;
        let fee = self.config.execution.fees.fee(notional, liquidity);
        match side {
            Side::Buy if notional + fee > self.portfolio.cash() => {
                self.cancel(id, CancelReason::InsufficientCash);
                return;
            }
            Side::Sell
                if !self.config.allow_short
                    && qty > self.portfolio.position(w.request.instrument) =>
            {
                self.cancel(id, CancelReason::InsufficientPosition);
                return;
            }
            _ => {}
        }

        let fill = Fill {
            order_id: id,
            ts: self.now,
            instrument: event.instrument(),
            side,
            price,
            qty,
            fee,
            liquidity,
        };
        self.portfolio.apply(&fill);
        if let Some(volume) = &mut capacity.bar_volume {
            *volume -= qty;
        }
        if liquidity == Liquidity::Maker
            && let Some(available) = &mut capacity.trade_qty
        {
            *available -= qty;
        }

        let w = self.working.get_mut(&id).expect("working");
        w.remaining -= qty;
        let remaining = w.remaining;
        if let Some(open) = self.open_orders.get_mut(&id) {
            open.remaining = remaining;
        }
        if let Some(record) = self.records.get_mut(&id) {
            record.filled += qty;
        }
        self.fills.push(fill.clone());
        self.notices.push(Notice::Fill(fill));
        if remaining.is_zero() {
            self.finish_order(id, OrderStatus::Filled);
            self.notices
                .push(Notice::Order(OrderUpdate::Filled { id, ts: self.now }));
        }
    }

    fn finish(&mut self, strategy: &mut dyn Strategy) {
        // Anything still working or in flight when the data runs out never fills.
        for Reverse((_, seq)) in std::mem::take(&mut self.schedule) {
            if let Some(Command::Submit(id, _)) = self.scheduled.remove(&seq) {
                self.cancel(id, CancelReason::EndOfData);
            }
        }
        let ids: Vec<OrderId> = self.working.keys().copied().collect();
        for id in ids {
            self.cancel(id, CancelReason::EndOfData);
        }
        self.sample_equity(true);
        self.deliver(strategy);
        self.callback(strategy, |s, ctx| s.on_finish(ctx));
        // Orders sent from on_finish are recorded, then cancelled unfilled.
        for Reverse((_, seq)) in std::mem::take(&mut self.schedule) {
            if let Some(Command::Submit(id, _)) = self.scheduled.remove(&seq) {
                self.finish_order(id, OrderStatus::Cancelled(CancelReason::EndOfData));
            }
        }
    }

    fn into_result(self, strategy: String) -> BacktestResult {
        let orders: Vec<OrderRecord> = self.records.into_values().collect();
        let run_hash = fingerprint(&orders, &self.fills, &self.equity);
        BacktestResult {
            strategy,
            config: self.config.clone(),
            span: self.span,
            events: self.events,
            final_cash: self.portfolio.cash(),
            final_positions: self.portfolio.positions().collect(),
            final_equity: self.portfolio.equity(&self.marks),
            fees_paid: self.portfolio.fees_paid(),
            orders,
            fills: self.fills,
            equity: self.equity,
            run_hash,
        }
    }
}

fn round_to_tick(price: Price, tick: Price, up: bool) -> Price {
    let (raw, step) = (price.raw(), tick.raw());
    let floor = raw.div_euclid(step) * step;
    Price::from_raw(if floor == raw || !up {
        floor
    } else {
        floor + step
    })
}

fn round_down(qty: Qty, lot: Qty) -> Qty {
    let step = lot.raw();
    Qty::from_raw(qty.raw().max(0).div_euclid(step) * step)
}
