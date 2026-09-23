//! Downloading archive files and converting them to Parquet.
//!
//! Layout under the data root mirrors the archive:
//! `binance/spot/klines/BTCUSDT/1m/BTCUSDT-1m-2024-01.{zip,parquet}`.
//! Downloads stream to a `.part` file, are checked against the SHA-256
//! checksum Binance publishes next to each file, and are renamed into
//! place only if they match. Existing files are reused.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tape_core::InstrumentId;
use zip::ZipArchive;

use crate::binance::{AggTradeReader, ArchiveFile, Dataset, KlineReader};
use crate::error::{DataError, Result};
use crate::store::{self, FileInfo};

pub struct Fetcher {
    root: PathBuf,
    agent: ureq::Agent,
}

impl Fetcher {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            agent: ureq::Agent::new_with_defaults(),
        }
    }

    pub fn zip_path(&self, file: &ArchiveFile) -> PathBuf {
        self.root
            .join(file.relative_dir())
            .join(format!("{}.zip", file.stem()))
    }

    pub fn parquet_path(&self, file: &ArchiveFile) -> PathBuf {
        self.root
            .join(file.relative_dir())
            .join(format!("{}.parquet", file.stem()))
    }

    /// Downloads and verifies a file, unless it's already on disk.
    pub fn download(&self, file: &ArchiveFile) -> Result<PathBuf> {
        let path = self.zip_path(file);
        if path.exists() {
            return Ok(path);
        }
        let url = file.url();
        let expected = self.fetch_checksum(file)?;

        fs::create_dir_all(path.parent().expect("zip path has a parent"))?;
        let part = path.with_extension("zip.part");
        let actual = {
            let response = self
                .agent
                .get(&url)
                .call()
                .map_err(|err| DataError::Download {
                    url: url.clone(),
                    message: err.to_string(),
                })?;
            let mut body = response.into_body().into_reader();
            let mut out = File::create(&part)?;
            let mut hasher = Sha256::new();
            let mut buf = vec![0; 256 * 1024];
            loop {
                let n = body.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                out.write_all(&buf[..n])?;
            }
            out.sync_all()?;
            hex::encode(hasher.finalize())
        };
        if actual != expected {
            let _ = fs::remove_file(&part);
            return Err(DataError::Checksum {
                file: file.stem(),
                expected,
                actual,
            });
        }
        fs::rename(&part, &path)?;
        Ok(path)
    }

    fn fetch_checksum(&self, file: &ArchiveFile) -> Result<String> {
        let url = file.checksum_url();
        let download_error = |message: String| DataError::Download {
            url: url.clone(),
            message,
        };
        let text = self
            .agent
            .get(&url)
            .call()
            .and_then(|mut response| response.body_mut().read_to_string())
            .map_err(|err| download_error(err.to_string()))?;
        parse_checksum(&text)
            .ok_or_else(|| download_error(format!("unexpected checksum file: {text:?}")))
    }

    /// Downloads a file if needed and converts it to Parquet, unless the
    /// Parquet file already exists.
    pub fn ingest(&self, file: &ArchiveFile) -> Result<FileInfo> {
        let parquet = self.parquet_path(file);
        if parquet.exists() {
            return store::file_info(&parquet);
        }
        let zip = self.download(file)?;
        ingest_zip(&zip, file, &parquet)
    }
}

/// A checksum file is `<sha256 hex>  <file name>`.
fn parse_checksum(text: &str) -> Option<String> {
    let hash = text.split_whitespace().next()?.to_ascii_lowercase();
    (hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hash)
}

/// Converts one archive zip (a single CSV) into a Parquet file, streaming
/// rows so month-long trade files never have to fit in memory.
pub fn ingest_zip(zip_path: &Path, file: &ArchiveFile, out: &Path) -> Result<FileInfo> {
    let mut archive = ZipArchive::new(File::open(zip_path)?)?;
    if archive.len() != 1 {
        return Err(DataError::Format {
            path: zip_path.to_owned(),
            message: format!("expected one CSV in the archive, found {}", archive.len()),
        });
    }
    let entry = archive.by_index(0)?;
    let name = entry.name().to_owned();
    // The instrument id isn't stored; readers assign their own.
    let id = InstrumentId(0);
    match &file.dataset {
        Dataset::Klines { .. } => {
            store::write_bars(out, &file.symbol, KlineReader::new(entry, id, name))
        }
        Dataset::AggTrades => {
            store::write_trades(out, &file.symbol, AggTradeReader::new(entry, id, name))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binance::Period;
    use std::io::Write;
    use tape_core::Timestamp;
    use zip::write::SimpleFileOptions;

    fn write_zip(path: &Path, entries: &[(&str, &str)]) {
        let mut zip = zip::ZipWriter::new(File::create(path).unwrap());
        for (name, body) in entries {
            zip.start_file(*name, SimpleFileOptions::default()).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    fn klines_file() -> ArchiveFile {
        ArchiveFile::new(
            "BTCUSDT",
            Dataset::Klines {
                interval: "1m".into(),
            },
            Period::Month {
                year: 2024,
                month: 1,
            },
        )
    }

    #[test]
    fn ingests_a_zipped_csv_into_parquet() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("in.zip");
        write_zip(
            &zip,
            &[(
                "BTCUSDT-1m-2024-01.csv",
                "1704067200000,42283.58,42298.62,42261.02,42298.61,35.92724,1704067259999,0,0,0,0,0\n\
                 1704067260000,42298.62,42320.00,42298.61,42320.00,21.07982,1704067319999,0,0,0,0,0\n",
            )],
        );
        let fetcher = Fetcher::new(dir.path());
        let out = fetcher.parquet_path(&klines_file());
        let info = ingest_zip(&zip, &klines_file(), &out).unwrap();
        assert_eq!(info.rows, 2);
        assert_eq!(info.symbol, "BTCUSDT");
        assert_eq!(
            info.span.unwrap().0,
            Timestamp::from_ymd_hms(2024, 1, 1, 0, 1, 0)
        );
        assert!(out.ends_with("binance/spot/klines/BTCUSDT/1m/BTCUSDT-1m-2024-01.parquet"));

        // A second ingest reuses the Parquet file instead of re-reading the zip.
        fs::remove_file(&zip).unwrap();
        assert_eq!(fetcher.ingest(&klines_file()).unwrap(), info);
    }

    #[test]
    fn rejects_archives_with_more_than_one_file() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("in.zip");
        write_zip(&zip, &[("a.csv", ""), ("b.csv", "")]);
        let err = ingest_zip(&zip, &klines_file(), &dir.path().join("out.parquet")).unwrap_err();
        assert!(err.to_string().contains("found 2"));
    }

    #[test]
    fn parses_checksum_files() {
        let hash = "a".repeat(64);
        assert_eq!(
            parse_checksum(&format!("{hash}  BTCUSDT-1m-2024-01.zip\n")),
            Some(hash)
        );
        assert_eq!(parse_checksum("<html>not found</html>"), None);
        assert_eq!(parse_checksum(""), None);
    }
}
