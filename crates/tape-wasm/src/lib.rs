//! Runs strategies compiled to WebAssembly inside a sandbox.
//!
//! The sandbox exists so that code nobody has reviewed, such as a strategy
//! an AI agent just wrote, can be backtested safely and fairly:
//!
//! - **Capabilities.** A module may import only the functions in
//!   [`tape_core::abi`]. Anything else, including WASI (clock, randomness,
//!   files, network), makes the module fail to load.
//! - **Compute.** Every callback gets the same fuel budget, roughly a count
//!   of WebAssembly instructions. An infinite loop ends the run with a clear
//!   error instead of hanging it.
//! - **Memory.** Linear memory is capped; growing past the cap traps.
//! - **Determinism.** NaNs are canonicalized, threads and relaxed SIMD are
//!   off, and nothing in the imports varies between runs, so the same module
//!   on the same data produces the same result on every machine.
//!
//! [`WasmStrategy`] implements the engine's `Strategy` trait. Before each
//! callback it copies what the strategy may read (time, cash, positions,
//! prices) into the sandbox; afterwards it replays the orders and cancels
//! the strategy sent through the real engine context.

use std::collections::BTreeMap;

use thiserror::Error;
use wasmtime::{
    Caller, Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder, Trap, TypedFunc,
};

use tape_core::{
    Fill, Instrument, InstrumentId, MarketEvent, Money, OrderId, OrderRequest, Price, Qty, abi,
};
use tape_engine::{Context, FeeModel, OrderUpdate, Strategy, affordable_qty};

/// Resource limits applied to every sandboxed strategy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Fuel (roughly, WebAssembly instructions) available per callback.
    pub fuel_per_callback: u64,
    /// Maximum linear memory, in bytes.
    pub memory_bytes: usize,
    /// Maximum log output over a whole run, in bytes.
    pub log_bytes: usize,
    /// Maximum orders and cancels in a single callback.
    pub commands_per_callback: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            fuel_per_callback: 10_000_000,
            memory_bytes: 64 << 20,
            log_bytes: 64 << 10,
            commands_per_callback: 1_000,
        }
    }
}

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("not a valid WebAssembly module: {0}")]
    Invalid(String),
    #[error("module imports `{0}`, which the sandbox does not provide")]
    ForbiddenImport(String),
    #[error("module doesn't export `{0}` with the expected signature")]
    MissingExport(String),
    #[error(
        "module was built for sandbox ABI version {found}; this host speaks version {expected}"
    )]
    AbiVersion { found: i32, expected: i32 },
    #[error("module failed while starting up: {0}")]
    Startup(String),
}

/// Host functions a module may import from the `tape` module.
const IMPORTS: [&str; 12] = [
    "now",
    "cash",
    "equity",
    "position",
    "last_price",
    "tick_size",
    "lot_size",
    "open_orders",
    "affordable_qty",
    "submit",
    "cancel",
    "log",
];

/// What the strategy may read during one callback.
#[derive(Default)]
struct Snapshot {
    now: i64,
    cash: i64,
    equity: i64,
    positions: BTreeMap<u32, i64>,
    marks: BTreeMap<u32, i64>,
    instruments: BTreeMap<u32, Instrument>,
    fees: Option<FeeModel>,
    open_orders: i32,
    next_order_id: u64,
}

enum GuestCommand {
    Submit(OrderRequest),
    Cancel(OrderId),
}

struct HostState {
    limits: StoreLimits,
    snapshot: Snapshot,
    outbox: Vec<GuestCommand>,
    commands_per_callback: usize,
    log: Vec<String>,
    log_bytes_left: usize,
}

/// `tape_on_bar(instrument, open_ts, close_ts, open, high, low, close, volume)`
type BarArgs = (i32, i64, i64, i64, i64, i64, i64, i64);
/// `tape_on_trade(instrument, ts, price, qty, aggressor)`
type TradeArgs = (i32, i64, i64, i64, i32);
/// `tape_on_fill(order_id, ts, instrument, side, price, qty, fee, liquidity)`
type FillArgs = (i64, i64, i32, i32, i64, i64, i64, i32);
/// `tape_on_order(order_id, ts, update, reason, remaining)`
type OrderArgs = (i64, i64, i32, i32, i64);

struct Exports {
    on_start: TypedFunc<(), ()>,
    on_bar: TypedFunc<BarArgs, ()>,
    on_trade: TypedFunc<TradeArgs, ()>,
    on_fill: TypedFunc<FillArgs, ()>,
    on_order: TypedFunc<OrderArgs, ()>,
    on_finish: TypedFunc<(), ()>,
}

/// Compiles and loads sandboxed strategies. Reuse one across many modules.
pub struct Sandbox {
    engine: Engine,
    linker: Linker<HostState>,
    limits: Limits,
}

fn deterministic_engine() -> Engine {
    let mut config = Config::new();
    config
        .consume_fuel(true)
        .cranelift_nan_canonicalization(true)
        .wasm_threads(false)
        .wasm_relaxed_simd(false);
    Engine::new(&config).expect("sandbox engine configuration is valid")
}

fn trap(message: impl Into<String>) -> wasmtime::Error {
    wasmtime::Error::msg(message.into())
}

fn instrument_id(code: i32) -> wasmtime::Result<InstrumentId> {
    u32::try_from(code)
        .map(InstrumentId)
        .map_err(|_| trap(format!("invalid instrument id {code}")))
}

fn build_linker(engine: &Engine) -> Linker<HostState> {
    let mut linker = Linker::new(engine);
    linker
        .func_wrap("tape", "now", |c: Caller<'_, HostState>| {
            c.data().snapshot.now
        })
        .and_then(|l| {
            l.func_wrap("tape", "cash", |c: Caller<'_, HostState>| {
                c.data().snapshot.cash
            })
        })
        .and_then(|l| {
            l.func_wrap("tape", "equity", |c: Caller<'_, HostState>| {
                c.data().snapshot.equity
            })
        })
        .and_then(|l| {
            l.func_wrap("tape", "position", |c: Caller<'_, HostState>, id: i32| {
                let id = u32::try_from(id).unwrap_or(u32::MAX);
                c.data().snapshot.positions.get(&id).copied().unwrap_or(0)
            })
        })
        .and_then(|l| {
            l.func_wrap("tape", "last_price", |c: Caller<'_, HostState>, id: i32| {
                let id = u32::try_from(id).unwrap_or(u32::MAX);
                c.data().snapshot.marks.get(&id).copied().unwrap_or(0)
            })
        })
        .and_then(|l| {
            l.func_wrap("tape", "tick_size", |c: Caller<'_, HostState>, id: i32| {
                let id = u32::try_from(id).unwrap_or(u32::MAX);
                c.data()
                    .snapshot
                    .instruments
                    .get(&id)
                    .map_or(0, |i| i.tick_size.raw())
            })
        })
        .and_then(|l| {
            l.func_wrap("tape", "lot_size", |c: Caller<'_, HostState>, id: i32| {
                let id = u32::try_from(id).unwrap_or(u32::MAX);
                c.data()
                    .snapshot
                    .instruments
                    .get(&id)
                    .map_or(0, |i| i.lot_size.raw())
            })
        })
        .and_then(|l| {
            l.func_wrap("tape", "open_orders", |c: Caller<'_, HostState>| {
                c.data().snapshot.open_orders
            })
        })
        .and_then(|l| {
            l.func_wrap(
                "tape",
                "affordable_qty",
                |c: Caller<'_, HostState>, id: i32, price: i64, cash: i64, margin_ppm: i64| {
                    let snapshot = &c.data().snapshot;
                    let id = u32::try_from(id).unwrap_or(u32::MAX);
                    let (Some(instrument), Some(fees)) =
                        (snapshot.instruments.get(&id), snapshot.fees.as_ref())
                    else {
                        return 0;
                    };
                    affordable_qty(
                        instrument,
                        fees,
                        Price::from_raw(price),
                        Money::from_raw(cash),
                        margin_ppm,
                    )
                    .raw()
                },
            )
        })
        .and_then(|l| {
            l.func_wrap(
                "tape",
                "submit",
                |mut c: Caller<'_, HostState>,
                 instrument: i32,
                 side: i32,
                 qty: i64,
                 kind: i32,
                 price: i64,
                 tif: i32|
                 -> wasmtime::Result<i64> {
                    let order = OrderRequest {
                        instrument: instrument_id(instrument)?,
                        side: abi::side_from(side)
                            .ok_or_else(|| trap(format!("invalid side {side}")))?,
                        qty: Qty::from_raw(qty),
                        order_type: abi::order_type_from(kind, price)
                            .ok_or_else(|| trap(format!("invalid order type {kind}")))?,
                        time_in_force: abi::tif_from(tif)
                            .ok_or_else(|| trap(format!("invalid time in force {tif}")))?,
                    };
                    let state = c.data_mut();
                    if state.outbox.len() >= state.commands_per_callback {
                        return Err(trap(format!(
                            "sent more than {} orders and cancels in one callback",
                            state.commands_per_callback
                        )));
                    }
                    // The engine assigns ids in sequence, so the id this
                    // order will get is known now.
                    let earlier = state
                        .outbox
                        .iter()
                        .filter(|cmd| matches!(cmd, GuestCommand::Submit(_)))
                        .count() as u64;
                    let id = state.snapshot.next_order_id + earlier;
                    state.outbox.push(GuestCommand::Submit(order));
                    Ok(id as i64)
                },
            )
        })
        .and_then(|l| {
            l.func_wrap(
                "tape",
                "cancel",
                |mut c: Caller<'_, HostState>, id: i64| -> wasmtime::Result<()> {
                    let state = c.data_mut();
                    if state.outbox.len() >= state.commands_per_callback {
                        return Err(trap("too many orders and cancels in one callback"));
                    }
                    let id =
                        u64::try_from(id).map_err(|_| trap(format!("invalid order id {id}")))?;
                    state.outbox.push(GuestCommand::Cancel(OrderId(id)));
                    Ok(())
                },
            )
        })
        .and_then(|l| {
            l.func_wrap(
                "tape",
                "log",
                |mut c: Caller<'_, HostState>, ptr: i32, len: i32| -> wasmtime::Result<()> {
                    let memory = c
                        .get_export("memory")
                        .and_then(|e| e.into_memory())
                        .ok_or_else(|| trap("module has no memory to log from"))?;
                    let (start, len) = (ptr as u32 as usize, len as u32 as usize);
                    let bytes = memory
                        .data(&c)
                        .get(start..start.saturating_add(len))
                        .ok_or_else(|| trap("log message is outside the module's memory"))?
                        .to_vec();
                    let state = c.data_mut();
                    let keep = bytes.len().min(state.log_bytes_left);
                    if keep > 0 {
                        state.log_bytes_left -= keep;
                        state
                            .log
                            .push(String::from_utf8_lossy(&bytes[..keep]).into_owned());
                    }
                    Ok(())
                },
            )
        })
        .expect("sandbox imports are defined once each");
    linker
}

impl Sandbox {
    pub fn new(limits: Limits) -> Self {
        let engine = deterministic_engine();
        let linker = build_linker(&engine);
        Self {
            engine,
            linker,
            limits,
        }
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Validates, compiles and instantiates a module (binary WebAssembly,
    /// or the text format).
    pub fn load(&self, module: impl AsRef<[u8]>) -> Result<WasmStrategy, SandboxError> {
        let module = Module::new(&self.engine, module.as_ref())
            .map_err(|err| SandboxError::Invalid(format!("{err:#}")))?;
        for import in module.imports() {
            if import.module() != "tape" || !IMPORTS.contains(&import.name()) {
                return Err(SandboxError::ForbiddenImport(format!(
                    "{}::{}",
                    import.module(),
                    import.name()
                )));
            }
        }

        let state = HostState {
            limits: StoreLimitsBuilder::new()
                .memory_size(self.limits.memory_bytes)
                .instances(1)
                .memories(1)
                .tables(4)
                .trap_on_grow_failure(true)
                .build(),
            snapshot: Snapshot::default(),
            outbox: Vec::new(),
            commands_per_callback: self.limits.commands_per_callback,
            log: Vec::new(),
            log_bytes_left: self.limits.log_bytes,
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|state| &mut state.limits);
        let startup = |err: wasmtime::Error| SandboxError::Startup(describe(&err, &self.limits));
        store
            .set_fuel(self.limits.fuel_per_callback)
            .map_err(startup)?;
        let instance = self
            .linker
            .instantiate(&mut store, &module)
            .map_err(startup)?;

        macro_rules! export {
            ($name:literal) => {
                instance
                    .get_typed_func(&mut store, $name)
                    .map_err(|_| SandboxError::MissingExport($name.into()))?
            };
        }
        let version: TypedFunc<(), i32> = export!("tape_abi_version");
        let found = version.call(&mut store, ()).map_err(startup)?;
        if found != abi::VERSION {
            return Err(SandboxError::AbiVersion {
                found,
                expected: abi::VERSION,
            });
        }

        let name_ptr: TypedFunc<(), i32> = export!("tape_name_ptr");
        let name_len: TypedFunc<(), i32> = export!("tape_name_len");
        let (ptr, len) = (
            name_ptr.call(&mut store, ()).map_err(startup)? as u32 as usize,
            name_len.call(&mut store, ()).map_err(startup)? as u32 as usize,
        );
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| SandboxError::MissingExport("memory".into()))?;
        let name = memory
            .data(&store)
            .get(ptr..ptr.saturating_add(len.min(200)))
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "wasm strategy".into());

        let limits = self.limits.clone();
        let exports = Exports {
            on_start: export!("tape_on_start"),
            on_bar: export!("tape_on_bar"),
            on_trade: export!("tape_on_trade"),
            on_fill: export!("tape_on_fill"),
            on_order: export!("tape_on_order"),
            on_finish: export!("tape_on_finish"),
        };
        Ok(WasmStrategy {
            store,
            exports,
            name,
            limits,
            fuel_used: 0,
            calls: 0,
            failed: false,
        })
    }
}

fn describe(err: &wasmtime::Error, limits: &Limits) -> String {
    match err.downcast_ref::<Trap>() {
        Some(Trap::OutOfFuel) => format!(
            "ran out of its compute budget ({} fuel per callback)",
            limits.fuel_per_callback
        ),
        Some(Trap::UnreachableCodeReached) => "panicked (reached unreachable code)".into(),
        Some(Trap::MemoryOutOfBounds) => "accessed memory out of bounds".into(),
        Some(other) => other.to_string(),
        // Host errors and limit violations arrive wrapped in a wasm
        // backtrace; the root cause is the useful part.
        None => {
            let cause = err.root_cause().to_string();
            if cause.contains("growing memory") {
                format!(
                    "went over its memory limit of {} MiB ({cause})",
                    limits.memory_bytes >> 20
                )
            } else {
                cause
            }
        }
    }
}

/// A loaded sandboxed strategy, ready to hand to the engine.
pub struct WasmStrategy {
    store: Store<HostState>,
    exports: Exports,
    name: String,
    limits: Limits,
    fuel_used: u64,
    calls: u64,
    failed: bool,
}

impl WasmStrategy {
    /// Lines the strategy logged, up to the log limit.
    pub fn log(&self) -> &[String] {
        &self.store.data().log
    }

    /// Total fuel consumed across all callbacks.
    pub fn fuel_used(&self) -> u64 {
        self.fuel_used
    }

    pub fn callbacks(&self) -> u64 {
        self.calls
    }

    fn snapshot(&mut self, ctx: &Context<'_>) {
        let snapshot = &mut self.store.data_mut().snapshot;
        if snapshot.fees.is_none() {
            snapshot.fees = Some(ctx.fees().clone());
            snapshot.instruments = ctx.instruments().map(|i| (i.id.0, i.clone())).collect();
        }
        snapshot.now = ctx.now().as_nanos();
        snapshot.cash = ctx.cash().raw();
        snapshot.equity = ctx.equity().raw();
        let ids: Vec<InstrumentId> = snapshot
            .instruments
            .keys()
            .map(|id| InstrumentId(*id))
            .collect();
        for id in ids {
            snapshot.positions.insert(id.0, ctx.position(id).raw());
            match ctx.last_price(id) {
                Some(price) => snapshot.marks.insert(id.0, price.raw()),
                None => snapshot.marks.remove(&id.0),
            };
        }
        snapshot.open_orders = i32::try_from(ctx.open_orders().count()).unwrap_or(i32::MAX);
        snapshot.next_order_id = ctx.next_order_id().0;
    }

    fn invoke(
        &mut self,
        ctx: &mut Context<'_>,
        callback: &str,
        call: impl FnOnce(&Exports, &mut Store<HostState>) -> wasmtime::Result<()>,
    ) {
        if self.failed {
            return;
        }
        self.snapshot(ctx);
        self.store.data_mut().outbox.clear();
        let fuel = self.limits.fuel_per_callback;
        let result = self
            .store
            .set_fuel(fuel)
            .and_then(|()| call(&self.exports, &mut self.store));
        self.fuel_used += fuel - self.store.get_fuel().unwrap_or(0);
        self.calls += 1;

        if let Err(err) = result {
            self.failed = true;
            ctx.abort(format!(
                "{} {} in {callback}",
                self.name,
                describe(&err, &self.limits)
            ));
            return;
        }
        for command in std::mem::take(&mut self.store.data_mut().outbox) {
            match command {
                GuestCommand::Submit(order) => {
                    ctx.submit(order);
                }
                GuestCommand::Cancel(id) => ctx.cancel(id),
            }
        }
    }
}

impl Strategy for WasmStrategy {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn on_start(&mut self, ctx: &mut Context<'_>) {
        self.invoke(ctx, "on_start", |e, s| e.on_start.call(s, ()));
    }

    fn on_market(&mut self, ctx: &mut Context<'_>, event: &MarketEvent) {
        match event {
            MarketEvent::Bar(bar) => {
                let args = (
                    bar.instrument.0 as i32,
                    bar.open_ts.as_nanos(),
                    bar.close_ts.as_nanos(),
                    bar.open.raw(),
                    bar.high.raw(),
                    bar.low.raw(),
                    bar.close.raw(),
                    bar.volume.raw(),
                );
                self.invoke(ctx, "on_bar", |e, s| e.on_bar.call(s, args));
            }
            MarketEvent::Trade(trade) => {
                let args = (
                    trade.instrument.0 as i32,
                    trade.ts.as_nanos(),
                    trade.price.raw(),
                    trade.qty.raw(),
                    abi::side_code(trade.aggressor),
                );
                self.invoke(ctx, "on_trade", |e, s| e.on_trade.call(s, args));
            }
        }
    }

    fn on_fill(&mut self, ctx: &mut Context<'_>, fill: &Fill) {
        let args = (
            fill.order_id.0 as i64,
            fill.ts.as_nanos(),
            fill.instrument.0 as i32,
            abi::side_code(fill.side),
            fill.price.raw(),
            fill.qty.raw(),
            fill.fee.raw(),
            abi::liquidity_code(fill.liquidity),
        );
        self.invoke(ctx, "on_fill", |e, s| e.on_fill.call(s, args));
    }

    fn on_order(&mut self, ctx: &mut Context<'_>, update: &OrderUpdate) {
        let args = match *update {
            OrderUpdate::Accepted { id, ts } => (
                id.0 as i64,
                ts.as_nanos(),
                abi::UPDATE_ACCEPTED,
                abi::NO_REASON,
                0,
            ),
            OrderUpdate::Rejected { id, ts, reason } => (
                id.0 as i64,
                ts.as_nanos(),
                abi::UPDATE_REJECTED,
                reason.abi_code(),
                0,
            ),
            OrderUpdate::Cancelled {
                id,
                ts,
                remaining,
                reason,
            } => (
                id.0 as i64,
                ts.as_nanos(),
                abi::UPDATE_CANCELLED,
                reason.abi_code(),
                remaining.raw(),
            ),
            OrderUpdate::Filled { id, ts } => (
                id.0 as i64,
                ts.as_nanos(),
                abi::UPDATE_FILLED,
                abi::NO_REASON,
                0,
            ),
        };
        self.invoke(ctx, "on_order", |e, s| e.on_order.call(s, args));
    }

    fn on_finish(&mut self, ctx: &mut Context<'_>) {
        self.invoke(ctx, "on_finish", |e, s| e.on_finish.call(s, ()));
    }
}

#[cfg(test)]
mod tests {
    use tape_engine::{CancelReason, RejectReason};

    #[test]
    fn reason_codes_match_the_abi_tables() {
        use tape_core::abi::{CANCEL_REASONS, REJECT_REASONS};
        assert_eq!(
            REJECT_REASONS[RejectReason::InsufficientCash.abi_code() as usize],
            "insufficient cash"
        );
        assert_eq!(
            REJECT_REASONS[RejectReason::InvalidPrice.abi_code() as usize],
            "invalid price"
        );
        assert_eq!(
            CANCEL_REASONS[CancelReason::EndOfData.abi_code() as usize],
            "end of data"
        );
        assert_eq!(
            CANCEL_REASONS[CancelReason::ImmediateOrCancel.abi_code() as usize],
            "immediate or cancel"
        );
    }
}
