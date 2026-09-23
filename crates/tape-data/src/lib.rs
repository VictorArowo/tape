//! Market data for tape.
//!
//! - [`binance`] parses Binance's public archive (bars and trades) into
//!   `tape-core` types, exactly.
//! - [`fetch`] downloads archive files, verifies their checksums and converts
//!   them to Parquet.
//! - [`store`] reads and writes the Parquet files.
//! - [`merge`] combines several time-ordered streams into one.

pub mod binance;
pub mod error;
pub mod fetch;
pub mod merge;
pub mod store;

pub use error::{DataError, Result};
pub use fetch::Fetcher;
pub use merge::Merge;
