//! Binance's public market data archive (data.binance.vision).
//!
//! The archive publishes one zipped CSV per symbol per day or month. Two
//! datasets matter here:
//!
//! - **klines**: OHLCV bars. Columns: open time, open, high, low, close,
//!   volume, close time, quote volume, trade count, taker buy base volume,
//!   taker buy quote volume, ignore.
//! - **aggTrades**: trades at the same price from one taker order, merged.
//!   Columns: aggregate trade id, price, quantity, first trade id, last trade
//!   id, timestamp, was the buyer the maker, was it the best price match.
//!
//! Spot files switched from millisecond to microsecond timestamps on
//! 2025-01-01, so the unit is detected per value. Some files have a header
//! row and some don't.

use std::io::Read;
use std::path::PathBuf;

use tape_core::{Bar, InstrumentId, Price, Qty, Side, TimeDelta, Timestamp, Trade};

use crate::error::{DataError, Result};

pub const BASE_URL: &str = "https://data.binance.vision";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dataset {
    /// Bars at an interval Binance publishes, e.g. `1m`, `1h`, `1d`.
    Klines {
        interval: String,
    },
    AggTrades,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Period {
    Month { year: i32, month: u32 },
    Day { year: i32, month: u32, day: u32 },
}

impl Period {
    fn cadence(self) -> &'static str {
        match self {
            Period::Month { .. } => "monthly",
            Period::Day { .. } => "daily",
        }
    }

    fn suffix(self) -> String {
        match self {
            Period::Month { year, month } => format!("{year:04}-{month:02}"),
            Period::Day { year, month, day } => format!("{year:04}-{month:02}-{day:02}"),
        }
    }
}

/// One file in the archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveFile {
    pub symbol: String,
    pub dataset: Dataset,
    pub period: Period,
}

impl ArchiveFile {
    pub fn new(symbol: impl Into<String>, dataset: Dataset, period: Period) -> Self {
        Self {
            symbol: symbol.into().to_uppercase(),
            dataset,
            period,
        }
    }

    /// File name without extension, e.g. `BTCUSDT-1m-2024-01`.
    pub fn stem(&self) -> String {
        match &self.dataset {
            Dataset::Klines { interval } => {
                format!("{}-{interval}-{}", self.symbol, self.period.suffix())
            }
            Dataset::AggTrades => format!("{}-aggTrades-{}", self.symbol, self.period.suffix()),
        }
    }

    /// Directory under the data root, mirroring the archive's layout.
    pub fn relative_dir(&self) -> PathBuf {
        let mut dir = PathBuf::from("binance/spot");
        match &self.dataset {
            Dataset::Klines { interval } => dir.extend(["klines", &self.symbol, interval]),
            Dataset::AggTrades => dir.extend(["aggTrades", &self.symbol]),
        }
        dir
    }

    pub fn url(&self) -> String {
        let dataset_path = match &self.dataset {
            Dataset::Klines { interval } => format!("klines/{}/{interval}", self.symbol),
            Dataset::AggTrades => format!("aggTrades/{}", self.symbol),
        };
        format!(
            "{BASE_URL}/data/spot/{}/{dataset_path}/{}.zip",
            self.period.cadence(),
            self.stem()
        )
    }

    pub fn checksum_url(&self) -> String {
        format!("{}.CHECKSUM", self.url())
    }
}

/// Microsecond timestamps for present-day dates are around 1.7e15;
/// millisecond ones are around 1.7e12. Anything above 1e14 is microseconds
/// (as milliseconds it would be the year 5138).
const MICROS_THRESHOLD: i64 = 100_000_000_000_000;

/// Converts a Binance timestamp and returns it with its unit.
pub fn parse_timestamp(raw: i64) -> (Timestamp, TimeDelta) {
    if raw.abs() >= MICROS_THRESHOLD {
        (Timestamp::from_micros(raw), TimeDelta::from_micros(1))
    } else {
        (Timestamp::from_millis(raw), TimeDelta::from_millis(1))
    }
}

struct Rows<R: Read> {
    records: csv::StringRecordsIntoIter<R>,
    source_name: String,
}

impl<R: Read> Rows<R> {
    fn new(reader: R, source_name: String) -> Self {
        let records = csv::ReaderBuilder::new()
            .has_headers(false)
            .flexible(true)
            .from_reader(reader)
            .into_records();
        Self {
            records,
            source_name,
        }
    }

    /// The next data row, skipping a header if the file has one.
    fn next_row(&mut self) -> Option<Result<(u64, csv::StringRecord)>> {
        loop {
            let record = match self.records.next()? {
                Ok(record) => record,
                Err(err) => return Some(Err(err.into())),
            };
            let line = record.position().map_or(0, |p| p.line());
            let first = record.get(0).unwrap_or("").trim();
            if first.is_empty() && record.len() <= 1 {
                continue;
            }
            if line == 1 && first.parse::<i64>().is_err() {
                continue;
            }
            return Some(Ok((line, record)));
        }
    }

    fn error(&self, line: u64, message: impl Into<String>) -> DataError {
        DataError::Parse {
            source_name: self.source_name.clone(),
            line,
            message: message.into(),
        }
    }

    fn field<T: std::str::FromStr>(
        &self,
        record: &csv::StringRecord,
        line: u64,
        index: usize,
        name: &str,
    ) -> Result<T>
    where
        T::Err: std::fmt::Display,
    {
        let raw = record
            .get(index)
            .ok_or_else(|| self.error(line, format!("missing column {name}")))?;
        raw.trim()
            .parse()
            .map_err(|err| self.error(line, format!("bad {name} {raw:?}: {err}")))
    }
}

/// Reads klines as [`Bar`]s. The file's inclusive close time becomes the
/// bar's exclusive `close_ts` (open + interval), which is also when the bar
/// becomes observable.
pub struct KlineReader<R: Read> {
    rows: Rows<R>,
    instrument: InstrumentId,
}

impl<R: Read> KlineReader<R> {
    pub fn new(reader: R, instrument: InstrumentId, source_name: impl Into<String>) -> Self {
        Self {
            rows: Rows::new(reader, source_name.into()),
            instrument,
        }
    }

    fn parse(&self, line: u64, record: &csv::StringRecord) -> Result<Bar> {
        let rows = &self.rows;
        let (open_ts, _) = parse_timestamp(rows.field(record, line, 0, "open time")?);
        let (last_ts, unit) = parse_timestamp(rows.field(record, line, 6, "close time")?);
        let bar = Bar {
            open_ts,
            close_ts: last_ts + unit,
            instrument: self.instrument,
            open: rows.field::<Price>(record, line, 1, "open")?,
            high: rows.field::<Price>(record, line, 2, "high")?,
            low: rows.field::<Price>(record, line, 3, "low")?,
            close: rows.field::<Price>(record, line, 4, "close")?,
            volume: rows.field::<Qty>(record, line, 5, "volume")?,
        };
        if !bar.is_consistent() {
            return Err(rows.error(line, format!("inconsistent bar: {bar:?}")));
        }
        Ok(bar)
    }
}

impl<R: Read> Iterator for KlineReader<R> {
    type Item = Result<Bar>;

    fn next(&mut self) -> Option<Self::Item> {
        let (line, record) = match self.rows.next_row()? {
            Ok(row) => row,
            Err(err) => return Some(Err(err)),
        };
        Some(self.parse(line, &record))
    }
}

/// Reads aggTrades as [`Trade`]s.
pub struct AggTradeReader<R: Read> {
    rows: Rows<R>,
    instrument: InstrumentId,
}

impl<R: Read> AggTradeReader<R> {
    pub fn new(reader: R, instrument: InstrumentId, source_name: impl Into<String>) -> Self {
        Self {
            rows: Rows::new(reader, source_name.into()),
            instrument,
        }
    }

    fn parse(&self, line: u64, record: &csv::StringRecord) -> Result<Trade> {
        let rows = &self.rows;
        let (ts, _) = parse_timestamp(rows.field(record, line, 5, "timestamp")?);
        let buyer_is_maker = match record.get(6).map(|s| s.trim().to_ascii_lowercase()) {
            Some(s) if s == "true" => true,
            Some(s) if s == "false" => false,
            other => {
                return Err(rows.error(line, format!("bad is_buyer_maker {other:?}")));
            }
        };
        let trade = Trade {
            ts,
            instrument: self.instrument,
            price: rows.field(record, line, 1, "price")?,
            qty: rows.field(record, line, 2, "quantity")?,
            // If the buyer's order was resting, the seller crossed the spread.
            aggressor: if buyer_is_maker {
                Side::Sell
            } else {
                Side::Buy
            },
        };
        if !trade.price.is_positive() || !trade.qty.is_positive() {
            return Err(rows.error(line, "price and quantity must be positive"));
        }
        Ok(trade)
    }
}

impl<R: Read> Iterator for AggTradeReader<R> {
    type Item = Result<Trade>;

    fn next(&mut self) -> Option<Self::Item> {
        let (line, record) = match self.rows.next_row()? {
            Ok(row) => row,
            Err(err) => return Some(Err(err)),
        };
        Some(self.parse(line, &record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KLINES_MS: &str = "\
1704067200000,42283.58,42298.62,42261.02,42298.61,35.92724000,1704067259999,1519214.21474050,1327,23.13111000,978164.42305390,0
1704067260000,42298.62,42320.00,42298.61,42320.00,21.07982000,1704067319999,891883.04853330,1094,13.86152000,586523.71226090,0
";

    const KLINES_US_WITH_HEADER: &str = "\
open_time,open,high,low,close,volume,close_time,quote_volume,count,taker_buy_volume,taker_buy_quote_volume,ignore
1735689600000000,93576.00,93610.93,93537.50,93610.93,8.21827000,1735689659999999,769013.39093480,1734,3.83947000,359235.08617370,0
";

    const AGG_TRADES: &str = "\
3229564458,42283.58000000,0.00134000,3339003585,3339003585,1704067200000,True,True
3229564459,42283.59000000,0.02366000,3339003586,3339003588,1704067200004,False,True
";

    fn bars(csv: &str) -> Vec<Bar> {
        KlineReader::new(csv.as_bytes(), InstrumentId(1), "test")
            .collect::<Result<_>>()
            .unwrap()
    }

    #[test]
    fn parses_millisecond_klines() {
        let bars = bars(KLINES_MS);
        assert_eq!(bars.len(), 2);
        let first = &bars[0];
        assert_eq!(first.open_ts, Timestamp::from_ymd(2024, 1, 1));
        assert_eq!(first.close_ts, Timestamp::from_ymd_hms(2024, 1, 1, 0, 1, 0));
        assert_eq!(first.open, "42283.58".parse().unwrap());
        assert_eq!(first.volume, "35.92724".parse().unwrap());
        assert_eq!(bars[1].open_ts, first.close_ts);
    }

    #[test]
    fn parses_microsecond_klines_and_skips_header() {
        let bars = bars(KLINES_US_WITH_HEADER);
        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].open_ts, Timestamp::from_ymd(2025, 1, 1));
        assert_eq!(bars[0].close_ts - bars[0].open_ts, TimeDelta::from_mins(1));
    }

    #[test]
    fn maps_buyer_maker_to_aggressor() {
        let trades: Vec<Trade> = AggTradeReader::new(AGG_TRADES.as_bytes(), InstrumentId(7), "t")
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(trades[0].aggressor, Side::Sell);
        assert_eq!(trades[1].aggressor, Side::Buy);
        assert_eq!(trades[1].ts - trades[0].ts, TimeDelta::from_millis(4));
        assert_eq!(trades[0].qty, "0.00134".parse().unwrap());
        assert_eq!(trades[0].instrument, InstrumentId(7));
    }

    #[test]
    fn reports_bad_rows_with_line_numbers() {
        let csv = format!("{KLINES_MS}1704067320000,abc,1,1,1,1,1704067379999,0,0,0,0,0\n");
        let err = KlineReader::new(csv.as_bytes(), InstrumentId(1), "BTCUSDT-1m-2024-01.csv")
            .find_map(Result::err)
            .unwrap();
        let message = err.to_string();
        assert!(
            message.contains("BTCUSDT-1m-2024-01.csv, line 3"),
            "{message}"
        );
        assert!(message.contains("bad open"), "{message}");
    }

    #[test]
    fn rejects_inconsistent_bars() {
        let csv = "1704067200000,100,99,98,100,1,1704067259999,0,0,0,0,0\n";
        let err = KlineReader::new(csv.as_bytes(), InstrumentId(1), "t")
            .next()
            .unwrap()
            .unwrap_err();
        assert!(err.to_string().contains("inconsistent bar"));
    }

    #[test]
    fn builds_archive_urls() {
        let monthly = ArchiveFile::new(
            "btcusdt",
            Dataset::Klines {
                interval: "1m".into(),
            },
            Period::Month {
                year: 2024,
                month: 1,
            },
        );
        assert_eq!(
            monthly.url(),
            "https://data.binance.vision/data/spot/monthly/klines/BTCUSDT/1m/BTCUSDT-1m-2024-01.zip"
        );
        assert_eq!(
            monthly.relative_dir(),
            PathBuf::from("binance/spot/klines/BTCUSDT/1m")
        );
        let daily = ArchiveFile::new(
            "ETHUSDT",
            Dataset::AggTrades,
            Period::Day {
                year: 2024,
                month: 3,
                day: 9,
            },
        );
        assert_eq!(
            daily.checksum_url(),
            "https://data.binance.vision/data/spot/daily/aggTrades/ETHUSDT/ETHUSDT-aggTrades-2024-03-09.zip.CHECKSUM"
        );
    }
}
