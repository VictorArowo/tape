//! What a backtest produces.

use serde::{Deserialize, Serialize};
use tape_core::{Fill, InstrumentId, Money, OrderId, OrderRequest, Qty, Timestamp};

use crate::config::BacktestConfig;
use crate::strategy::{CancelReason, RejectReason};

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderStatus {
    InFlight,
    Working,
    Filled,
    Cancelled(CancelReason),
    Rejected(RejectReason),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderRecord {
    pub id: OrderId,
    pub request: OrderRequest,
    pub submitted_at: Timestamp,
    pub arrived_at: Option<Timestamp>,
    pub finished_at: Option<Timestamp>,
    pub filled: Qty,
    pub status: OrderStatus,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EquityPoint {
    pub ts: Timestamp,
    pub equity: Money,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BacktestResult {
    pub strategy: String,
    pub config: BacktestConfig,
    /// Time of the first and last market events.
    pub span: Option<(Timestamp, Timestamp)>,
    pub events: u64,
    pub orders: Vec<OrderRecord>,
    pub fills: Vec<Fill>,
    pub equity: Vec<EquityPoint>,
    pub final_cash: Money,
    pub final_positions: Vec<(InstrumentId, Qty)>,
    pub final_equity: Money,
    pub fees_paid: Money,
    /// Fingerprint of every order, fill and equity point. Two runs with the
    /// same data, strategy and config must produce the same hash.
    pub run_hash: String,
}

/// 64-bit FNV-1a: tiny, dependency-free and identical on every platform.
/// Not cryptographic; it only needs to detect differences between runs.
pub(crate) fn fingerprint(
    orders: &[OrderRecord],
    fills: &[Fill],
    equity: &[EquityPoint],
) -> String {
    let bytes = serde_json::to_vec(&(orders, fills, equity)).expect("report serializes");
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("{hash:016x}")
}
