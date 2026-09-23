//! Core types shared by every part of tape: fixed-point numbers,
//! timestamps, market data events, orders and fills.
//!
//! Nothing here knows about the engine or where data comes from, so the same
//! types can be compiled into sandboxed strategies later on.

pub mod abi;
pub mod fixed;
pub mod instrument;
pub mod market;
pub mod order;
pub mod time;

pub use fixed::{Money, ParseFixedError, Price, Qty};
pub use instrument::{Instrument, InstrumentId};
pub use market::{Bar, MarketEvent, Side, Trade};
pub use order::{Fill, Liquidity, OrderId, OrderRequest, OrderType, TimeInForce};
pub use time::{TimeDelta, Timestamp};
