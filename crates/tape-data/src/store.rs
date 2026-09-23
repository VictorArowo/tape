//! Parquet storage for bars and trades.
//!
//! Prices, quantities and timestamps are stored as their raw `i64` values
//! (10^-8 units and nanoseconds), so a round trip is bit-exact. Each file
//! holds one symbol in time order and records what it contains in its
//! key-value metadata. Files are written to a temporary path and renamed
//! into place, so a crash never leaves a half-written file that looks
//! complete.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::{Array, BooleanArray, Int64Array, RecordBatch};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;

use tape_core::{Bar, InstrumentId, Price, Qty, Side, Timestamp, Trade};

use crate::error::{DataError, Result};

const FORMAT_VERSION: &str = "1";
const BATCH_ROWS: usize = 64 * 1024;

const KEY_FORMAT: &str = "tape.format";
const KEY_KIND: &str = "tape.kind";
const KEY_SYMBOL: &str = "tape.symbol";
const KEY_ROWS: &str = "tape.rows";
const KEY_FIRST_TS: &str = "tape.first_ts";
const KEY_LAST_TS: &str = "tape.last_ts";

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Bars,
    Trades,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Bars => "bars",
            Kind::Trades => "trades",
        }
    }
}

/// What a stored file contains, read from its metadata without scanning it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileInfo {
    pub path: PathBuf,
    pub kind: Kind,
    pub symbol: String,
    pub rows: u64,
    /// Event time of the first and last rows, if the file isn't empty.
    pub span: Option<(Timestamp, Timestamp)>,
}

fn bar_schema() -> SchemaRef {
    let col = |name| Field::new(name, DataType::Int64, false);
    Arc::new(Schema::new(vec![
        col("open_ts"),
        col("close_ts"),
        col("open"),
        col("high"),
        col("low"),
        col("close"),
        col("volume"),
    ]))
}

fn trade_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("ts", DataType::Int64, false),
        Field::new("price", DataType::Int64, false),
        Field::new("qty", DataType::Int64, false),
        Field::new("buyer_aggressor", DataType::Boolean, false),
    ]))
}

/// Writes rows of one kind to a temporary file, then renames it into place.
struct Writer {
    // `None` once `finish` has closed it.
    writer: Option<ArrowWriter<File>>,
    tmp_path: PathBuf,
    path: PathBuf,
    kind: Kind,
    symbol: String,
    rows: u64,
    first: Option<Timestamp>,
    last: Option<Timestamp>,
}

impl Writer {
    fn create(path: &Path, kind: Kind, symbol: &str, schema: SchemaRef) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp_path = path.with_extension("parquet.tmp");
        let props = WriterProperties::builder()
            .set_compression(Compression::ZSTD(ZstdLevel::try_new(3)?))
            .build();
        let writer = ArrowWriter::try_new(File::create(&tmp_path)?, schema, Some(props))?;
        Ok(Self {
            writer: Some(writer),
            tmp_path,
            path: path.to_owned(),
            kind,
            symbol: symbol.to_owned(),
            rows: 0,
            first: None,
            last: None,
        })
    }

    /// Enforces time order while writing, so bad data fails at ingest
    /// instead of silently reordering events during a backtest.
    fn observe(&mut self, ts: Timestamp) -> Result<()> {
        if let Some(last) = self.last
            && ts < last
        {
            return Err(DataError::OutOfOrder {
                source_name: self.path.display().to_string(),
                previous: last,
                next: ts,
            });
        }
        self.first.get_or_insert(ts);
        self.last = Some(ts);
        self.rows += 1;
        Ok(())
    }

    fn write(&mut self, batch: &RecordBatch) -> Result<()> {
        let writer = self.writer.as_mut().expect("writer used after finish");
        writer.write(batch)?;
        Ok(())
    }

    fn finish(mut self) -> Result<FileInfo> {
        let mut meta = vec![
            (KEY_FORMAT, FORMAT_VERSION.to_owned()),
            (KEY_KIND, self.kind.as_str().to_owned()),
            (KEY_SYMBOL, self.symbol.clone()),
            (KEY_ROWS, self.rows.to_string()),
        ];
        if let (Some(first), Some(last)) = (self.first, self.last) {
            meta.push((KEY_FIRST_TS, first.as_nanos().to_string()));
            meta.push((KEY_LAST_TS, last.as_nanos().to_string()));
        }
        let mut writer = self.writer.take().expect("writer finished twice");
        for (key, value) in meta {
            writer.append_key_value_metadata(KeyValue::new(key.to_owned(), value));
        }
        writer.close()?;
        fs::rename(&self.tmp_path, &self.path)?;
        Ok(FileInfo {
            path: self.path.clone(),
            kind: self.kind,
            symbol: self.symbol.clone(),
            rows: self.rows,
            span: self.first.zip(self.last),
        })
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // Only reached with the temp file still present if writing failed.
        let _ = fs::remove_file(&self.tmp_path);
    }
}

fn int_column(values: Vec<i64>) -> Arc<dyn Array> {
    Arc::new(Int64Array::from(values))
}

/// Writes bars for one symbol, in time order, to `path`.
pub fn write_bars(
    path: &Path,
    symbol: &str,
    bars: impl IntoIterator<Item = Result<Bar>>,
) -> Result<FileInfo> {
    let schema = bar_schema();
    let mut writer = Writer::create(path, Kind::Bars, symbol, schema.clone())?;
    let mut bars = bars.into_iter().peekable();
    while bars.peek().is_some() {
        let mut cols: [Vec<i64>; 7] = Default::default();
        for bar in bars.by_ref().take(BATCH_ROWS) {
            let bar = bar?;
            writer.observe(bar.close_ts)?;
            let values = [
                bar.open_ts.as_nanos(),
                bar.close_ts.as_nanos(),
                bar.open.raw(),
                bar.high.raw(),
                bar.low.raw(),
                bar.close.raw(),
                bar.volume.raw(),
            ];
            for (col, value) in cols.iter_mut().zip(values) {
                col.push(value);
            }
        }
        let batch = RecordBatch::try_new(schema.clone(), cols.map(int_column).to_vec())?;
        writer.write(&batch)?;
    }
    writer.finish()
}

/// Writes trades for one symbol, in time order, to `path`.
pub fn write_trades(
    path: &Path,
    symbol: &str,
    trades: impl IntoIterator<Item = Result<Trade>>,
) -> Result<FileInfo> {
    let schema = trade_schema();
    let mut writer = Writer::create(path, Kind::Trades, symbol, schema.clone())?;
    let mut trades = trades.into_iter().peekable();
    while trades.peek().is_some() {
        let (mut ts, mut price, mut qty, mut buyer) = (vec![], vec![], vec![], vec![]);
        for trade in trades.by_ref().take(BATCH_ROWS) {
            let trade = trade?;
            writer.observe(trade.ts)?;
            ts.push(trade.ts.as_nanos());
            price.push(trade.price.raw());
            qty.push(trade.qty.raw());
            buyer.push(trade.aggressor == Side::Buy);
        }
        let columns = vec![
            int_column(ts),
            int_column(price),
            int_column(qty),
            Arc::new(BooleanArray::from(buyer)) as Arc<dyn Array>,
        ];
        writer.write(&RecordBatch::try_new(schema.clone(), columns)?)?;
    }
    writer.finish()
}

fn format_error(path: &Path, message: impl Into<String>) -> DataError {
    DataError::Format {
        path: path.to_owned(),
        message: message.into(),
    }
}

/// Reads a stored file's metadata without scanning its rows.
pub fn file_info(path: &Path) -> Result<FileInfo> {
    let builder = ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)?;
    info_from_metadata(path, &builder)
}

fn info_from_metadata(
    path: &Path,
    builder: &ParquetRecordBatchReaderBuilder<File>,
) -> Result<FileInfo> {
    let kv = builder
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .ok_or_else(|| format_error(path, "not a tape file (no metadata)"))?;
    let get = |key: &str| {
        kv.iter()
            .find(|entry| entry.key == key)
            .and_then(|entry| entry.value.clone())
    };
    let require = |key: &str| get(key).ok_or_else(|| format_error(path, format!("missing {key}")));

    let format = require(KEY_FORMAT)?;
    if format != FORMAT_VERSION {
        return Err(format_error(
            path,
            format!("unsupported format version {format}"),
        ));
    }
    let kind = match require(KEY_KIND)?.as_str() {
        "bars" => Kind::Bars,
        "trades" => Kind::Trades,
        other => return Err(format_error(path, format!("unknown kind {other}"))),
    };
    let parse_i64 = |key: &str| -> Result<Option<i64>> {
        get(key)
            .map(|v| {
                v.parse()
                    .map_err(|_| format_error(path, format!("bad {key}")))
            })
            .transpose()
    };
    let rows = parse_i64(KEY_ROWS)?.unwrap_or(0) as u64;
    let span = match (parse_i64(KEY_FIRST_TS)?, parse_i64(KEY_LAST_TS)?) {
        (Some(first), Some(last)) => {
            Some((Timestamp::from_nanos(first), Timestamp::from_nanos(last)))
        }
        _ => None,
    };
    Ok(FileInfo {
        path: path.to_owned(),
        kind,
        symbol: require(KEY_SYMBOL)?,
        rows,
        span,
    })
}

fn open(path: &Path, expected: Kind) -> Result<(FileInfo, ParquetRecordBatchReader)> {
    let builder = ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)?;
    let info = info_from_metadata(path, &builder)?;
    if info.kind != expected {
        return Err(format_error(
            path,
            format!(
                "expected {}, found {}",
                expected.as_str(),
                info.kind.as_str()
            ),
        ));
    }
    let reader = builder.with_batch_size(BATCH_ROWS).build()?;
    Ok((info, reader))
}

fn int_col<'a>(path: &Path, batch: &'a RecordBatch, name: &str) -> Result<&'a Int64Array> {
    batch
        .column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
        .ok_or_else(|| format_error(path, format!("missing int64 column {name}")))
}

/// Streams rows out of a stored file one batch at a time.
struct BatchRows<T> {
    path: PathBuf,
    reader: ParquetRecordBatchReader,
    decode: fn(&Path, &RecordBatch, InstrumentId) -> Result<Vec<T>>,
    instrument: InstrumentId,
    buffer: std::vec::IntoIter<T>,
}

impl<T> Iterator for BatchRows<T> {
    type Item = Result<T>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(row) = self.buffer.next() {
                return Some(Ok(row));
            }
            let batch = match self.reader.next()? {
                Ok(batch) => batch,
                Err(err) => return Some(Err(err.into())),
            };
            match (self.decode)(&self.path, &batch, self.instrument) {
                Ok(rows) => self.buffer = rows.into_iter(),
                Err(err) => return Some(Err(err)),
            }
        }
    }
}

fn decode_bars(path: &Path, batch: &RecordBatch, instrument: InstrumentId) -> Result<Vec<Bar>> {
    let [open_ts, close_ts, open, high, low, close, volume] = [
        "open_ts", "close_ts", "open", "high", "low", "close", "volume",
    ]
    .map(|name| int_col(path, batch, name));
    let (open_ts, close_ts, open, high, low, close, volume) =
        (open_ts?, close_ts?, open?, high?, low?, close?, volume?);
    Ok((0..batch.num_rows())
        .map(|i| Bar {
            open_ts: Timestamp::from_nanos(open_ts.value(i)),
            close_ts: Timestamp::from_nanos(close_ts.value(i)),
            instrument,
            open: Price::from_raw(open.value(i)),
            high: Price::from_raw(high.value(i)),
            low: Price::from_raw(low.value(i)),
            close: Price::from_raw(close.value(i)),
            volume: Qty::from_raw(volume.value(i)),
        })
        .collect())
}

fn decode_trades(path: &Path, batch: &RecordBatch, instrument: InstrumentId) -> Result<Vec<Trade>> {
    let ts = int_col(path, batch, "ts")?;
    let price = int_col(path, batch, "price")?;
    let qty = int_col(path, batch, "qty")?;
    let buyer = batch
        .column_by_name("buyer_aggressor")
        .and_then(|c| c.as_any().downcast_ref::<BooleanArray>())
        .ok_or_else(|| format_error(path, "missing boolean column buyer_aggressor"))?;
    Ok((0..batch.num_rows())
        .map(|i| Trade {
            ts: Timestamp::from_nanos(ts.value(i)),
            instrument,
            price: Price::from_raw(price.value(i)),
            qty: Qty::from_raw(qty.value(i)),
            aggressor: if buyer.value(i) {
                Side::Buy
            } else {
                Side::Sell
            },
        })
        .collect())
}

/// Opens a bars file and streams its rows, tagged with `instrument`.
pub fn read_bars(
    path: &Path,
    instrument: InstrumentId,
) -> Result<(FileInfo, impl Iterator<Item = Result<Bar>> + use<>)> {
    let (info, reader) = open(path, Kind::Bars)?;
    let rows = BatchRows {
        path: path.to_owned(),
        reader,
        decode: decode_bars,
        instrument,
        buffer: Vec::new().into_iter(),
    };
    Ok((info, rows))
}

/// Opens a trades file and streams its rows, tagged with `instrument`.
pub fn read_trades(
    path: &Path,
    instrument: InstrumentId,
) -> Result<(FileInfo, impl Iterator<Item = Result<Trade>> + use<>)> {
    let (info, reader) = open(path, Kind::Trades)?;
    let rows = BatchRows {
        path: path.to_owned(),
        reader,
        decode: decode_trades,
        instrument,
        buffer: Vec::new().into_iter(),
    };
    Ok((info, rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tape_core::TimeDelta;

    fn sample_bars(n: usize) -> Vec<Bar> {
        let start = Timestamp::from_ymd(2024, 1, 1);
        (0..n)
            .map(|i| {
                let open_ts = start + TimeDelta::from_mins(i as i64);
                let base = Price::from_raw(4_200_000_000_000 + i as i64 * 1_000_001);
                Bar {
                    open_ts,
                    close_ts: open_ts + TimeDelta::from_mins(1),
                    instrument: InstrumentId(3),
                    open: base,
                    high: base + Price::from_raw(5_000),
                    low: base - Price::from_raw(7_000),
                    close: base + Price::from_raw(1),
                    volume: Qty::from_raw(123_456_789 + i as i64),
                }
            })
            .collect()
    }

    #[test]
    fn bars_round_trip_exactly_across_batches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bars/BTCUSDT-1m.parquet");
        let bars = sample_bars(BATCH_ROWS * 2 + 17);
        let info = write_bars(&path, "BTCUSDT", bars.iter().cloned().map(Ok)).unwrap();
        assert_eq!(info.rows, bars.len() as u64);
        assert_eq!(
            info.span,
            Some((bars[0].close_ts, bars.last().unwrap().close_ts))
        );
        assert!(!path.with_extension("parquet.tmp").exists());

        let (read_info, rows) = read_bars(&path, InstrumentId(3)).unwrap();
        assert_eq!(read_info, info);
        let back: Vec<Bar> = rows.collect::<Result<_>>().unwrap();
        assert_eq!(back, bars);
    }

    #[test]
    fn trades_round_trip_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.parquet");
        let start = Timestamp::from_ymd(2025, 6, 1);
        let trades: Vec<Trade> = (0..1_000)
            .map(|i| Trade {
                ts: start + TimeDelta::from_micros(i * 37),
                instrument: InstrumentId(9),
                price: Price::from_raw(6_500_000_000_000 + i),
                qty: Qty::from_raw(1 + i * 3),
                aggressor: if i % 3 == 0 { Side::Sell } else { Side::Buy },
            })
            .collect();
        write_trades(&path, "BTCUSDT", trades.iter().cloned().map(Ok)).unwrap();
        let (info, rows) = read_trades(&path, InstrumentId(9)).unwrap();
        assert_eq!(info.kind, Kind::Trades);
        assert_eq!(rows.collect::<Result<Vec<_>>>().unwrap(), trades);
    }

    #[test]
    fn refuses_out_of_order_rows_and_leaves_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.parquet");
        let mut bars = sample_bars(3);
        bars.swap(1, 2);
        let err = write_bars(&path, "BTCUSDT", bars.into_iter().map(Ok)).unwrap_err();
        assert!(matches!(err, DataError::OutOfOrder { .. }), "{err}");
        assert!(!path.exists());
        assert!(!path.with_extension("parquet.tmp").exists());
    }

    #[test]
    fn rejects_reading_the_wrong_kind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.parquet");
        write_bars(&path, "BTCUSDT", sample_bars(2).into_iter().map(Ok)).unwrap();
        let err = read_trades(&path, InstrumentId(1)).err().unwrap();
        assert!(err.to_string().contains("expected trades, found bars"));
    }

    #[test]
    fn empty_files_have_no_span() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.parquet");
        let info = write_bars(&path, "BTCUSDT", std::iter::empty()).unwrap();
        assert_eq!((info.rows, info.span), (0, None));
        assert_eq!(file_info(&path).unwrap(), info);
    }
}
