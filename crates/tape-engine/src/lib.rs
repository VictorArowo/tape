//! An event-driven backtesting engine that is deterministic and hard to
//! fool.
//!
//! Feed it a time-ordered stream of [`MarketEvent`](tape_core::MarketEvent)s
//! and a [`Strategy`]; it simulates an exchange with latency, fees,
//! slippage and volume limits, and returns every order, fill and equity
//! point along with a fingerprint of the run.

pub mod config;
pub mod engine;
pub mod portfolio;
pub mod report;
pub mod strategy;

pub use config::{BacktestConfig, ExecutionModel, FeeModel};
pub use engine::{Backtest, EngineError};
pub use portfolio::Portfolio;
pub use report::{BacktestResult, EquityPoint, OrderRecord, OrderStatus};
pub use strategy::{
    CancelReason, Context, OpenOrder, OrderState, OrderUpdate, RejectReason, Strategy,
    affordable_qty,
};
