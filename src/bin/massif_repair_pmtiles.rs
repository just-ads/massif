#![allow(dead_code)]

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

#[path = "../container/mod.rs"]
mod container;
#[path = "../encoder.rs"]
mod encoder;
#[path = "../frontier.rs"]
mod frontier;
#[path = "../pipeline.rs"]
mod pipeline;
#[path = "../pmtiles/mod.rs"]
mod pmtiles;
#[path = "../progress.rs"]
mod progress;
#[path = "../raster.rs"]
mod raster;
#[path = "../tile.rs"]
mod tile;
#[path = "../tile_format/mod.rs"]
mod tile_format;

use encoder::Encoding;
use tile_format::TileFormat;

fn parse_read_buffer_size(value: &str) -> std::result::Result<usize, String> {
    match value {
        "1024" => Ok(1024),
        "2048" => Ok(2048),
        _ => Err("read buffer size must be either 1024 or 2048".to_owned()),
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "massif-repair-pmtiles",
    version,
    about = "Regenerate skipped PMTiles temp tiles from a failure log and rebuild the archive"
)]
pub(crate) struct Args {
    /// Input elevation raster used by the original massif run.
    pub(crate) input: PathBuf,

    /// Output .pmtiles file from the original massif run.
    pub(crate) output: PathBuf,

    /// Skipped tile log path. Defaults to {output}.skipped_pmtiles_tiles.log.
    #[arg(long, value_name = "PATH")]
    pub(crate) skipped_tiles_log: Option<PathBuf>,

    /// Base elevation offset — must match the original run.
    #[arg(short = 'b', long, default_value = "-10000", allow_hyphen_values = true)]
    pub(crate) base_val: f64,

    /// Elevation interval / precision in metres — must match the original run.
    #[arg(short = 'i', long, default_value = "0.1")]
    pub(crate) interval: f64,

    /// Zero out the lowest N bits of the encoded integer — must match the original run.
    #[arg(short = 'r', long, default_value = "3")]
    pub(crate) round_digits: u32,

    /// Minimum zoom level from the original run.
    #[arg(long, default_value = "5")]
    pub(crate) min_z: u8,

    /// Maximum zoom level from the original run.
    #[arg(long, default_value = "12")]
    pub(crate) max_z: u8,

    /// RGB encoding scheme from the original run.
    #[arg(long, value_enum, default_value = "mapbox")]
    pub(crate) encoding: Encoding,

    /// Output tile format from the original run.
    #[arg(long, value_enum, default_value = "webp")]
    pub(crate) format: TileFormat,

    /// Compression level from the original run.
    #[arg(long, value_name = "LEVEL", value_parser = clap::value_parser!(u8).range(1..=9))]
    pub(crate) compress: Option<u8>,

    /// Override nodata value from the original run.
    #[arg(long, allow_hyphen_values = true)]
    pub(crate) nodata: Option<f32>,

    /// Elevation threshold from the original run.
    #[arg(long, allow_hyphen_values = true)]
    pub(crate) zero_below: Option<f32>,

    /// Fill uniform descendants setting from the original run.
    #[arg(long)]
    pub(crate) fill_uniform_descendants: bool,

    /// Worker thread count.
    #[arg(short = 'j', long)]
    pub(crate) workers: Option<usize>,

    /// Maximum source read buffer size per tile.
    #[arg(long, default_value = "2048", value_parser = parse_read_buffer_size)]
    pub(crate) read_buffer_size: usize,

    /// Keep {output}.tmp after rebuilding the PMTiles archive.
    #[arg(long)]
    pub(crate) keep_temp: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    if args.output.extension().and_then(|ext| ext.to_str()) != Some("pmtiles") {
        anyhow::bail!("output must be a .pmtiles file");
    }

    if args.min_z > args.max_z {
        anyhow::bail!("--min-z must be <= --max-z");
    }

    if args.max_z > 31 {
        anyhow::bail!("--max-z must be <= 31 because tile coordinates are stored as u32");
    }

    if let Some(w) = args.workers {
        rayon::ThreadPoolBuilder::new()
            .num_threads(w)
            .build_global()
            .context("build rayon thread pool")?;
    }

    let input_str = args
        .input
        .to_str()
        .context("input path is not valid UTF-8")?
        .to_owned();

    pmtiles::flow::regenerate_skipped_tiles(&args, &input_str, args.skipped_tiles_log.as_deref())
}
