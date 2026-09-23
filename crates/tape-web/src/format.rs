//! A compact binary format for a month of bars, for shipping to browsers.
//!
//! Parquet would need a decoder in the page, and JSON is five times the
//! size. This is fixed-width little-endian integers: a 64-byte header, then
//! 28 bytes per bar. Prices are stored in the instrument's tick increments
//! (cents for BTCUSDT), which is exact because every price is a multiple
//! of the tick. Volumes are stored as tape's raw 10^-8 units: exchanges
//! report them more finely than their lot size (BTCUSDT volumes have six
//! decimals in places), and a large minute can exceed 32 bits. Decoding
//! gives back exactly the values that were encoded.
//!
//! ```text
//! 0   8  magic "TAPEBARS"
//! 8   4  version (1)
//! 12  4  bar count
//! 16  8  open time of the first bar, ns since 1970
//! 24  4  bar interval, seconds
//! 28  1  price decimals      29  1  quantity decimals   30  2  reserved
//! 32  8  tick size, raw      40  8  lot size, raw
//! 48 16  symbol, ASCII, zero-padded
//! then per bar: interval offset u32, open i32, high i32, low i32, close i32, volume i64 (raw)
//! ```

use tape_core::{Bar, Instrument, InstrumentId, Price, Qty, TimeDelta, Timestamp};

const MAGIC: &[u8; 8] = b"TAPEBARS";
const VERSION: u32 = 1;
const HEADER: usize = 64;
const RECORD: usize = 28;

#[derive(Debug)]
pub struct Dataset {
    pub instrument: Instrument,
    pub bars: Vec<Bar>,
}

/// Decimal places needed to express `raw` (a 10^-8 value) exactly.
fn decimals_of(raw: i64) -> u8 {
    let mut decimals = 8;
    let mut value = raw;
    while decimals > 0 && value % 10 == 0 {
        value /= 10;
        decimals -= 1;
    }
    decimals
}

fn scale(decimals: u8) -> i64 {
    10_i64.pow(u32::from(8 - decimals))
}

/// Encodes consecutive bars of one instrument and interval.
pub fn encode(instrument: &Instrument, bars: &[Bar]) -> Result<Vec<u8>, String> {
    let first = bars.first().ok_or("no bars to encode")?;
    let interval = first.close_ts - first.open_ts;
    let interval_secs = interval.as_nanos() / 1_000_000_000;
    if interval_secs <= 0 || interval.as_nanos() % 1_000_000_000 != 0 {
        return Err("bar interval must be a whole number of seconds".into());
    }
    let price_decimals = decimals_of(instrument.tick_size.raw());
    let price_scale = scale(price_decimals);
    let qty_decimals = 8;

    let mut out = Vec::with_capacity(HEADER + RECORD * bars.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(
        &u32::try_from(bars.len())
            .map_err(|_| "too many bars")?
            .to_le_bytes(),
    );
    out.extend_from_slice(&first.open_ts.as_nanos().to_le_bytes());
    out.extend_from_slice(&(interval_secs as u32).to_le_bytes());
    out.extend_from_slice(&[price_decimals, qty_decimals, 0, 0]);
    out.extend_from_slice(&instrument.tick_size.raw().to_le_bytes());
    out.extend_from_slice(&instrument.lot_size.raw().to_le_bytes());
    let mut symbol = [0u8; 16];
    let name = instrument.symbol.as_bytes();
    if name.len() > 16 || !name.is_ascii() {
        return Err(format!(
            "symbol {} doesn't fit in 16 ASCII bytes",
            instrument.symbol
        ));
    }
    symbol[..name.len()].copy_from_slice(name);
    out.extend_from_slice(&symbol);

    let narrow = |raw: i64, unit: i64, what: &str| -> Result<i32, String> {
        if raw % unit != 0 {
            return Err(format!("{what} {raw} isn't a whole number of increments"));
        }
        i32::try_from(raw / unit).map_err(|_| format!("{what} {raw} is too large"))
    };
    for bar in bars {
        if bar.close_ts - bar.open_ts != interval {
            return Err(format!("bar at {} has a different interval", bar.open_ts));
        }
        let offset = (bar.open_ts - first.open_ts).as_nanos() / interval.as_nanos();
        let offset = u32::try_from(offset).map_err(|_| "bars are out of order")?;
        out.extend_from_slice(&offset.to_le_bytes());
        for price in [bar.open, bar.high, bar.low, bar.close] {
            out.extend_from_slice(&narrow(price.raw(), price_scale, "price")?.to_le_bytes());
        }
        if bar.volume.is_negative() {
            return Err(format!("bar at {} has negative volume", bar.open_ts));
        }
        out.extend_from_slice(&bar.volume.raw().to_le_bytes());
    }
    Ok(out)
}

fn read<const N: usize>(bytes: &[u8], at: usize) -> [u8; N] {
    bytes[at..at + N]
        .try_into()
        .expect("bounds checked by the caller")
}

/// Decodes a file written by [`encode`]. The instrument gets id 1.
pub fn decode(bytes: &[u8]) -> Result<Dataset, String> {
    if bytes.len() < HEADER || &bytes[..8] != MAGIC {
        return Err("not a tape bars file".into());
    }
    let version = u32::from_le_bytes(read(bytes, 8));
    if version != VERSION {
        return Err(format!("unsupported bars file version {version}"));
    }
    let count = u32::from_le_bytes(read(bytes, 12)) as usize;
    if bytes.len() != HEADER + count * RECORD {
        return Err(format!("bars file is truncated (expected {count} bars)"));
    }
    let start = Timestamp::from_nanos(i64::from_le_bytes(read(bytes, 16)));
    let interval = TimeDelta::from_secs(i64::from(u32::from_le_bytes(read(bytes, 24))));
    let (price_decimals, qty_decimals) = (bytes[28], bytes[29]);
    if price_decimals > 8 || qty_decimals != 8 {
        return Err("bad decimals in header".into());
    }
    let price_scale = scale(price_decimals);
    let tick = Price::from_raw(i64::from_le_bytes(read(bytes, 32)));
    let lot = Qty::from_raw(i64::from_le_bytes(read(bytes, 40)));
    if !tick.is_positive() || !lot.is_positive() {
        return Err("bad tick or lot size in header".into());
    }
    let symbol: String = bytes[48..64]
        .iter()
        .take_while(|b| **b != 0)
        .map(|b| char::from(*b))
        .collect();
    let instrument = Instrument::new(InstrumentId(1), symbol, tick, lot);

    let mut bars = Vec::with_capacity(count);
    let mut last_offset = None;
    for i in 0..count {
        let at = HEADER + i * RECORD;
        let offset = u32::from_le_bytes(read(bytes, at));
        if last_offset.is_some_and(|last| offset <= last) {
            return Err(format!("bar {i} is out of order"));
        }
        last_offset = Some(offset);
        let field = |n: usize| i64::from(i32::from_le_bytes(read(bytes, at + 4 + 4 * n)));
        let open_ts = start + TimeDelta::from_nanos(interval.as_nanos() * i64::from(offset));
        let bar = Bar {
            open_ts,
            close_ts: open_ts + interval,
            instrument: instrument.id,
            open: Price::from_raw(field(0) * price_scale),
            high: Price::from_raw(field(1) * price_scale),
            low: Price::from_raw(field(2) * price_scale),
            close: Price::from_raw(field(3) * price_scale),
            volume: Qty::from_raw(i64::from_le_bytes(read(bytes, at + 20))),
        };
        if !bar.is_consistent() {
            return Err(format!("bar {i} is inconsistent"));
        }
        bars.push(bar);
    }
    Ok(Dataset { instrument, bars })
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sample() -> (Instrument, Vec<Bar>) {
        let instrument = Instrument::new(
            InstrumentId(1),
            "BTCUSDT",
            "0.01".parse().unwrap(),
            "0.00001".parse().unwrap(),
        );
        let start = Timestamp::from_ymd(2024, 1, 1);
        let bars = (0..100)
            .filter(|i| i % 17 != 5) // leave gaps, as real data sometimes has
            .map(|i: i64| {
                let open_ts = start + TimeDelta::from_mins(i);
                let base = 4_228_358_000_000 + i * 1_000_000 * 37;
                Bar {
                    open_ts,
                    close_ts: open_ts + TimeDelta::from_mins(1),
                    instrument: InstrumentId(1),
                    open: Price::from_raw(base),
                    high: Price::from_raw(base + 5_000_000),
                    low: Price::from_raw(base - 3_000_000),
                    close: Price::from_raw(base + 1_000_000),
                    volume: Qty::from_raw(3_592_724_000 + i * 1_000),
                }
            })
            .collect();
        (instrument, bars)
    }

    #[test]
    fn round_trips_exactly_including_gaps() {
        let (instrument, bars) = sample();
        let bytes = encode(&instrument, &bars).unwrap();
        assert_eq!(bytes.len(), HEADER + RECORD * bars.len());
        let back = decode(&bytes).unwrap();
        assert_eq!(back.bars, bars);
        assert_eq!(back.instrument, instrument);
    }

    #[test]
    fn keeps_volumes_finer_than_the_lot_and_larger_than_32_bits() {
        let (instrument, mut bars) = sample();
        bars[3].volume = Qty::from_raw(93_621_215_700); // 936.212157 BTC, six decimals
        bars[4].volume = Qty::from_int(5_000).unwrap(); // over 2^31 raw units
        let back = decode(&encode(&instrument, &bars).unwrap()).unwrap();
        assert_eq!(back.bars, bars);
    }

    #[test]
    fn refuses_prices_off_the_tick() {
        let (instrument, mut bars) = sample();
        bars[3].close = Price::from_raw(bars[3].close.raw() + 1);
        bars[3].high = bars[3].high.max(bars[3].close);
        assert!(encode(&instrument, &bars).unwrap_err().contains("price"));
    }

    #[test]
    fn rejects_corrupt_files() {
        let (instrument, bars) = sample();
        let bytes = encode(&instrument, &bars).unwrap();
        assert!(
            decode(&bytes[..bytes.len() - 1])
                .unwrap_err()
                .contains("truncated")
        );
        assert!(
            decode(b"not a bars file at all, really, nope, no no no no no no no no no").is_err()
        );
    }
}
