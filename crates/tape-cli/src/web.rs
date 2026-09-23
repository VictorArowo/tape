//! `tape web`: data and checks for the in-browser demo.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use clap::Args;
use tape_core::MarketEvent;
use tape_data::binance::Period;

use crate::run::{instrument, load};

#[derive(Args)]
pub struct ExportArgs {
    #[arg(long, default_value = "BTCUSDT")]
    symbol: String,
    /// First month, as YYYY-MM.
    #[arg(long)]
    pub from: String,
    /// Last month (inclusive), as YYYY-MM.
    #[arg(long)]
    pub to: Option<String>,
    /// Directory for the `<symbol>-<YYYY-MM>.bin` files.
    #[arg(long, default_value = "web/dist/data")]
    out: PathBuf,
}

#[derive(Args)]
pub struct WebRunArgs {
    /// A month file written by `tape web export`.
    #[arg(long)]
    data: PathBuf,
    /// The page's parameters, as JSON.
    #[arg(long)]
    params: String,
}

fn bars(data_dir: &Path, symbol: &str, period: Period) -> Result<Vec<tape_core::Bar>> {
    load(data_dir, symbol, "1m", &[period])?
        .map(|event| match event {
            Ok(MarketEvent::Bar(bar)) => Ok(bar),
            Ok(_) => anyhow::bail!("expected bars"),
            Err(err) => Err(err.into()),
        })
        .collect()
}

pub fn export(data_dir: &Path, args: ExportArgs, periods: Vec<Period>) -> Result<()> {
    let instrument = instrument(&args.symbol)?;
    fs::create_dir_all(&args.out)?;
    for period in periods {
        let Period::Month { year, month } = period else {
            continue;
        };
        let bars = bars(data_dir, &args.symbol, period)?;
        let bytes = tape_web::format::encode(&instrument, &bars)
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("couldn't encode {year}-{month:02}"))?;
        // Decode what was written and compare, so a file is never shipped
        // unless it round-trips exactly.
        let back = tape_web::format::decode(&bytes).map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            back.bars == bars,
            "{year}-{month:02} didn't round-trip exactly"
        );
        let path = args.out.join(format!(
            "{}-{year:04}-{month:02}.bin",
            args.symbol.to_lowercase()
        ));
        fs::write(&path, &bytes)?;
        println!(
            "{}  {} bars  {} bytes",
            path.display(),
            bars.len(),
            bytes.len()
        );
    }
    Ok(())
}

pub fn run(args: WebRunArgs) -> Result<()> {
    let bytes =
        fs::read(&args.data).with_context(|| format!("couldn't read {}", args.data.display()))?;
    let dataset = tape_web::format::decode(&bytes).map_err(anyhow::Error::msg)?;
    println!("{}", tape_web::demo::run_json(Some(&dataset), &args.params));
    Ok(())
}
