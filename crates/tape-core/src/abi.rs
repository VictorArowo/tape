//! The boundary between the engine and a strategy compiled to WebAssembly.
//!
//! Only plain integers cross it. Prices, quantities and money travel as
//! their raw fixed-point values, timestamps as nanoseconds, and enums as the
//! small codes below. The host (`tape-wasm`) and the guest SDK
//! (`tape-guest`) both use these definitions, so they can't drift apart.
//!
//! Host functions the guest may import, all from the `tape` module:
//!
//! | import | signature | returns |
//! |---|---|---|
//! | `now` | `() -> i64` | current time |
//! | `cash`, `equity` | `() -> i64` | account values |
//! | `position` | `(instrument: i32) -> i64` | quantity held |
//! | `last_price` | `(instrument: i32) -> i64` | last price, or 0 if none |
//! | `tick_size`, `lot_size` | `(instrument: i32) -> i64` | 0 if unknown |
//! | `open_orders` | `() -> i32` | orders sent and not finished |
//! | `affordable_qty` | `(instrument: i32, price: i64, cash: i64, margin_ppm: i64) -> i64` | quantity |
//! | `submit` | `(instrument: i32, side: i32, qty: i64, kind: i32, price: i64, tif: i32) -> i64` | order id |
//! | `cancel` | `(order_id: i64)` | |
//! | `log` | `(ptr: i32, len: i32)` | |
//!
//! Functions the guest exports: `tape_abi_version() -> i32`,
//! `tape_name_ptr() -> i32`, `tape_name_len() -> i32`, `tape_on_start()`,
//! `tape_on_bar(instrument: i32, open_ts, close_ts, open, high, low, close,
//! volume: i64)`, `tape_on_trade(instrument: i32, ts: i64, price: i64, qty:
//! i64, aggressor: i32)`, `tape_on_fill(order_id: i64, ts: i64, instrument:
//! i32, side: i32, price: i64, qty: i64, fee: i64, liquidity: i32)`,
//! `tape_on_order(order_id: i64, ts: i64, update: i32, reason: i32,
//! remaining: i64)` and `tape_on_finish()`, plus its `memory`.
//!
//! There is deliberately no import for wall-clock time, randomness, files or
//! the network.

use crate::market::Side;
use crate::order::{Liquidity, OrderType, TimeInForce};

pub const VERSION: i32 = 1;

pub const SIDE_BUY: i32 = 0;
pub const SIDE_SELL: i32 = 1;

pub const ORDER_MARKET: i32 = 0;
pub const ORDER_LIMIT: i32 = 1;
pub const ORDER_STOP: i32 = 2;

pub const TIF_GTC: i32 = 0;
pub const TIF_IOC: i32 = 1;

pub const LIQUIDITY_MAKER: i32 = 0;
pub const LIQUIDITY_TAKER: i32 = 1;

pub const UPDATE_ACCEPTED: i32 = 0;
pub const UPDATE_REJECTED: i32 = 1;
pub const UPDATE_CANCELLED: i32 = 2;
pub const UPDATE_FILLED: i32 = 3;

/// Reason codes for rejected orders, in this order.
pub const REJECT_REASONS: [&str; 5] = [
    "unknown instrument",
    "invalid quantity",
    "invalid price",
    "insufficient cash",
    "insufficient position",
];

/// Reason codes for cancelled orders, in this order.
pub const CANCEL_REASONS: [&str; 5] = [
    "requested",
    "immediate or cancel",
    "insufficient cash",
    "insufficient position",
    "end of data",
];

pub const NO_REASON: i32 = -1;

pub fn side_code(side: Side) -> i32 {
    match side {
        Side::Buy => SIDE_BUY,
        Side::Sell => SIDE_SELL,
    }
}

pub fn side_from(code: i32) -> Option<Side> {
    match code {
        SIDE_BUY => Some(Side::Buy),
        SIDE_SELL => Some(Side::Sell),
        _ => None,
    }
}

pub fn liquidity_code(liquidity: Liquidity) -> i32 {
    match liquidity {
        Liquidity::Maker => LIQUIDITY_MAKER,
        Liquidity::Taker => LIQUIDITY_TAKER,
    }
}

pub fn liquidity_from(code: i32) -> Option<Liquidity> {
    match code {
        LIQUIDITY_MAKER => Some(Liquidity::Maker),
        LIQUIDITY_TAKER => Some(Liquidity::Taker),
        _ => None,
    }
}

pub fn tif_code(tif: TimeInForce) -> i32 {
    match tif {
        TimeInForce::Gtc => TIF_GTC,
        TimeInForce::Ioc => TIF_IOC,
    }
}

pub fn tif_from(code: i32) -> Option<TimeInForce> {
    match code {
        TIF_GTC => Some(TimeInForce::Gtc),
        TIF_IOC => Some(TimeInForce::Ioc),
        _ => None,
    }
}

/// An order type as its code and price argument (0 for market orders).
pub fn order_type_code(order_type: OrderType) -> (i32, i64) {
    match order_type {
        OrderType::Market => (ORDER_MARKET, 0),
        OrderType::Limit { price } => (ORDER_LIMIT, price.raw()),
        OrderType::Stop { trigger } => (ORDER_STOP, trigger.raw()),
    }
}

pub fn order_type_from(code: i32, price: i64) -> Option<OrderType> {
    let price = crate::Price::from_raw(price);
    match code {
        ORDER_MARKET => Some(OrderType::Market),
        ORDER_LIMIT => Some(OrderType::Limit { price }),
        ORDER_STOP => Some(OrderType::Stop { trigger: price }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Price;

    #[test]
    fn codes_round_trip() {
        for side in [Side::Buy, Side::Sell] {
            assert_eq!(side_from(side_code(side)), Some(side));
        }
        for tif in [TimeInForce::Gtc, TimeInForce::Ioc] {
            assert_eq!(tif_from(tif_code(tif)), Some(tif));
        }
        for liquidity in [Liquidity::Maker, Liquidity::Taker] {
            assert_eq!(liquidity_from(liquidity_code(liquidity)), Some(liquidity));
        }
        let price = Price::from_raw(123);
        for order_type in [
            OrderType::Market,
            OrderType::Limit { price },
            OrderType::Stop { trigger: price },
        ] {
            let (code, raw) = order_type_code(order_type);
            assert_eq!(order_type_from(code, raw), Some(order_type));
        }
        assert_eq!(side_from(7), None);
        assert_eq!(order_type_from(9, 0), None);
    }
}
