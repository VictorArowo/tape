mod run;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use tape_data::Fetcher;
use tape_data::binance::{ArchiveFile, Dataset, Period};
use tape_data::store::{self, FileInfo};

#[derive(Parser)]
#[command(
    name = "tape",
    version,
    about = "A backtesting engine that doesn't lie"
)]
struct Cli {
    /// Where market data is stored.
    #[arg(long, global = true, default_value = "data", env = "TAPE_DATA_DIR")]
    data_dir: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Download and inspect market data.
    #[command(subcommand)]
    Data(DataCommand),
    /// Backtest a reference strategy on stored data.
    Run(run::RunArgs),
}

#[derive(Subcommand)]
enum DataCommand {
    /// Download monthly files from Binance's public archive and convert
    /// them to Parquet. Files already on disk are reused.
    Fetch(FetchArgs),
    /// Show what a stored Parquet file contains.
    Info { paths: Vec<PathBuf> },
}

#[derive(Args)]
struct FetchArgs {
    /// Symbol, e.g. BTCUSDT.
    symbol: String,
    /// First month, as YYYY-MM.
    #[arg(long)]
    from: String,
    /// Last month (inclusive), as YYYY-MM. Defaults to --from.
    #[arg(long)]
    to: Option<String>,
    /// Bar interval, e.g. 1m or 1h. Mutually exclusive with --trades.
    #[arg(long, conflicts_with = "trades", default_value = "1m")]
    klines: String,
    /// Fetch aggregated trades instead of bars. Much larger files.
    #[arg(long)]
    trades: bool,
}

fn parse_month(s: &str) -> Result<(i32, u32)> {
    let (year, month) = s
        .split_once('-')
        .with_context(|| format!("expected YYYY-MM, got {s:?}"))?;
    let year: i32 = year.parse().with_context(|| format!("bad year in {s:?}"))?;
    let month: u32 = month
        .parse()
        .with_context(|| format!("bad month in {s:?}"))?;
    if !(1..=12).contains(&month) {
        bail!("month out of range in {s:?}");
    }
    Ok((year, month))
}

fn months(from: (i32, u32), to: (i32, u32)) -> Result<Vec<Period>> {
    if to < from {
        bail!("--to is before --from");
    }
    let mut out = Vec::new();
    let (mut year, mut month) = from;
    while (year, month) <= to {
        out.push(Period::Month { year, month });
        (year, month) = if month == 12 {
            (year + 1, 1)
        } else {
            (year, month + 1)
        };
    }
    Ok(out)
}

fn print_info(info: &FileInfo) {
    let span = match info.span {
        Some((first, last)) => format!("{first} → {last}"),
        None => "empty".to_owned(),
    };
    println!(
        "{}  {:?} {}  {} rows  {span}",
        info.path.display(),
        info.kind,
        info.symbol,
        info.rows
    );
}

fn fetch(data_dir: PathBuf, args: FetchArgs) -> Result<()> {
    let from = parse_month(&args.from)?;
    let to = args
        .to
        .as_deref()
        .map(parse_month)
        .transpose()?
        .unwrap_or(from);
    let dataset = if args.trades {
        Dataset::AggTrades
    } else {
        Dataset::Klines {
            interval: args.klines,
        }
    };
    let fetcher = Fetcher::new(data_dir);
    for period in months(from, to)? {
        let file = ArchiveFile::new(&args.symbol, dataset.clone(), period);
        eprintln!("fetching {}", file.stem());
        let info = fetcher
            .ingest(&file)
            .with_context(|| format!("failed to fetch {}", file.stem()))?;
        print_info(&info);
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Data(DataCommand::Fetch(args)) => fetch(cli.data_dir, args),
        Command::Run(args) => {
            let from = parse_month(args.from())?;
            let to = args.to().map(parse_month).transpose()?.unwrap_or(from);
            run::run(&cli.data_dir, args, months(from, to)?)
        }
        Command::Data(DataCommand::Info { paths }) => {
            for path in paths {
                print_info(&store::file_info(&path)?);
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_ranges_cross_year_boundaries() {
        let periods = months((2024, 11), (2025, 2)).unwrap();
        assert_eq!(periods.len(), 4);
        assert_eq!(
            periods[2],
            Period::Month {
                year: 2025,
                month: 1
            }
        );
        assert!(months((2025, 1), (2024, 12)).is_err());
        assert!(parse_month("2024-13").is_err());
        assert!(parse_month("2024").is_err());
    }
}
