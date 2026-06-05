use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Parser;

mod mbtiles;
mod container;
mod encoder;
mod frontier;
mod pipeline;
mod pmtiles;
mod progress;
mod raster;
mod tile;
mod tile_format;

use encoder::Encoding;
use raster::dataset_wgs84_bounds;
use tile::{lat_to_tile_y_xyz, lon_to_tile_x};
use tile_format::TileFormat;

fn parse_read_buffer_size(value: &str) -> std::result::Result<usize, String> {
    match value {
        "1024" => Ok(1024),
        "2048" => Ok(2048),
        _ => Err("read buffer size must be either 1024 or 2048".to_owned()),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutputKind {
    Pmtiles,
    Mbtiles,
}

fn output_kind(path: &Path) -> Result<OutputKind> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("pmtiles") => Ok(OutputKind::Pmtiles),
        Some("mbtiles") => Ok(OutputKind::Mbtiles),
        other => bail!("Unknown output extension {:?} — use .pmtiles or .mbtiles", other),
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "massif",
    version,
    about = "Fast terrain-RGB tile generator — converts elevation rasters to PMTiles or MBTiles"
)]
pub(crate) struct Args {
    /// Input elevation raster — GeoTIFF, VRT, or any GDAL-supported format and CRS
    pub(crate) input: PathBuf,

    /// Output file — .pmtiles or .mbtiles (container inferred from extension)
    pub(crate) output: PathBuf,

    /// Base elevation offset — Mapbox decode: height = base_val + (R·65536+G·256+B) · interval
    #[arg(short = 'b', long, default_value = "-10000", allow_hyphen_values = true)]
    pub(crate) base_val: f64,

    /// Elevation interval / precision in metres
    #[arg(short = 'i', long, default_value = "0.1")]
    pub(crate) interval: f64,

    /// Zero out the lowest N bits of the encoded integer (rio-rgbify -r)
    #[arg(short = 'r', long, default_value = "3")]
    pub(crate) round_digits: u32,

    /// Minimum zoom level to generate
    #[arg(long, default_value = "5")]
    pub(crate) min_z: u8,

    /// Maximum zoom level to generate
    #[arg(long, default_value = "12")]
    pub(crate) max_z: u8,

    /// RGB encoding scheme [default: mapbox]
    #[arg(long, value_enum, default_value = "mapbox")]
    pub(crate) encoding: Encoding,

    /// Output tile format [default: webp]
    #[arg(long, value_enum, default_value = "webp")]
    pub(crate) format: TileFormat,

    /// Compression level 1–9 (omit for fastest; 6 is a good default).
    /// Higher = smaller file, slower encoding. Format-agnostic — maps to the
    /// best available compressor for the output format.
    #[arg(long, value_name = "LEVEL", value_parser = clap::value_parser!(u8).range(1..=9))]
    pub(crate) compress: Option<u8>,

    /// Override the nodata value from the raster metadata.
    /// Useful when the file has no embedded nodata or it is wrong (common values: 0, -9999, -32768).
    #[arg(long, allow_hyphen_values = true)]
    pub(crate) nodata: Option<f32>,

    /// Elevation threshold — values below this are encoded as 0.
    /// Example: --zero-below -100.0  (all elevations < -100 are encoded as 0)
    #[arg(long, allow_hyphen_values = true)]
    pub(crate) zero_below: Option<f32>,

    /// Fill all descendants of a uniform encoded tile with that tile's RGB value.
    /// This is a lossy heuristic; local higher-zoom variation below the parent tile can be skipped.
    #[arg(long)]
    pub(crate) fill_uniform_descendants: bool,

    /// Worker thread count (default: all CPUs)
    #[arg(short = 'j', long)]
    pub(crate) workers: Option<usize>,

    /// Maximum source read buffer size per tile: 1024 is faster/smaller, 2048 is more conservative.
    #[arg(long, default_value = "2048", value_parser = parse_read_buffer_size)]
    pub(crate) read_buffer_size: usize,

    /// PMTiles only: keep the temporary tile pyramid directory after writing the final archive.
    #[arg(long)]
    pub(crate) keep_temp: bool,

    /// PMTiles only: skip tile generation and build the archive from the existing {output}.tmp tile pyramid.
    #[arg(long)]
    pub(crate) build_from_temp: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    if args.min_z > args.max_z {
        bail!("--min-z must be <= --max-z");
    }

    if args.max_z > 31 {
        bail!("--max-z must be <= 31 because tile coordinates are stored as u32");
    }

    if let Some(w) = args.workers {
        rayon::ThreadPoolBuilder::new()
            .num_threads(w)
            .build_global()
            .context("build rayon thread pool")?;
    }

    let output_kind = output_kind(&args.output)?;

    if args.keep_temp && output_kind != OutputKind::Pmtiles {
        bail!("--keep-temp is only supported for .pmtiles output");
    }

    if args.build_from_temp {
        if output_kind != OutputKind::Pmtiles {
            bail!("--build-from-temp is only supported for .pmtiles output");
        }
        return pmtiles::flow::build_from_temp(&args);
    }

    let input_str = args
        .input
        .to_str()
        .context("input path is not valid UTF-8")?
        .to_owned();

    let (west_lon, south_lat, east_lon, north_lat) = dataset_wgs84_bounds(&args.input)?;

    let mut upper_bound_tiles: usize = 0;
    for z in args.min_z..=args.max_z {
        let x0 = lon_to_tile_x(west_lon, z);
        let x1 = lon_to_tile_x(east_lon, z);
        let y0 = lat_to_tile_y_xyz(north_lat, z);
        let y1 = lat_to_tile_y_xyz(south_lat, z);
        upper_bound_tiles += ((x1 - x0 + 1) as usize) * ((y1 - y0 + 1) as usize);
    }

    eprintln!(
        "Zoom {}-{}: up to {} candidate tiles, sparse frontier enabled ({} threads)",
        args.min_z,
        args.max_z,
        upper_bound_tiles,
        rayon::current_num_threads()
    );

    if args.encoding == Encoding::Terrarium {
        if args.base_val != -10000.0 {
            eprintln!("Warning: --base-val is ignored for --encoding terrarium");
        }
        if args.interval != 0.1 {
            eprintln!("Warning: --interval is ignored for --encoding terrarium");
        }
        if args.round_digits != 3 {
            eprintln!("Warning: --round-digits is ignored for --encoding terrarium");
        }
    }

    let bounds = (west_lon, south_lat, east_lon, north_lat);
    match output_kind {
        OutputKind::Pmtiles => pmtiles::flow::run(&args, &input_str, bounds, upper_bound_tiles),
        OutputKind::Mbtiles => mbtiles::flow::run(&args, &input_str, bounds, upper_bound_tiles),
    }
}
