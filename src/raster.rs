use std::cell::RefCell;
use std::path::Path;

use anyhow::{Context, Result};
use gdal::spatial_ref::{AxisMappingStrategy, CoordTransform, SpatialRef};
use gdal::Dataset;

use crate::encoder::{encode_mapbox, encode_terrarium, Encoding};
use crate::tile_format::TileFormat;
use crate::tile::{merc_to_wgs84, tile_bounds_3857, HALF_CIRC};

const TILE_SIZE: usize = 512;
const SKIRT: usize = 1;
const GRID_SIZE: usize = TILE_SIZE + 2 * SKIRT;
const GRID_PIXELS: usize = GRID_SIZE * GRID_SIZE;

pub(crate) struct ProcessedTile {
    pub(crate) data: Vec<u8>,
    pub(crate) uniform_color: Option<[u8; 3]>,
}

/// Bilinear sample from a flat f32 buffer. Returns `nodata` if out of bounds.
/// Elevation thresholding is applied after sampling during encoding.
pub fn sample_bilinear(
    data: &[f32],
    width: usize,
    height: usize,
    px: f64,
    py: f64,
    nodata: f32,
) -> f32 {
    if px < 0.0 || py < 0.0 || px >= width as f64 || py >= height as f64 {
        return nodata;
    }
    let x0 = px.floor() as usize;
    let y0 = py.floor() as usize;
    let x1 = (x0 + 1).min(width - 1);
    let y1 = (y0 + 1).min(height - 1);
    let fx = px - x0 as f64;
    let fy = py - y0 as f64;

    let v = [
        data[y0 * width + x0],
        data[y0 * width + x1],
        data[y1 * width + x0],
        data[y1 * width + x1],
    ];

    let is_nd = |v: f32| (v - nodata).abs() < 0.5 || v.is_nan();
    if v.iter().any(|&s| is_nd(s)) {
        // Nearest-neighbour fallback
        let nx = if fx < 0.5 { x0 } else { x1 };
        let ny = if fy < 0.5 { y0 } else { y1 };
        return data[ny * width + nx];
    }

    (v[0] as f64 * (1.0 - fx) * (1.0 - fy)
        + v[1] as f64 * fx * (1.0 - fy)
        + v[2] as f64 * (1.0 - fx) * fy
        + v[3] as f64 * fx * fy) as f32
}

/// Read the WGS84 bounding box of a GDAL dataset.
/// Returns (west_lon, south_lat, east_lon, north_lat).
pub fn dataset_wgs84_bounds(path: &Path) -> Result<(f64, f64, f64, f64)> {
    let ds = Dataset::open(path).context("open dataset")?;
    let gt = ds.geo_transform().context("geo_transform")?;
    let (w, h) = ds.raster_size();

    let ox = gt[0];
    let oy = gt[3];
    let ex = ox + gt[1] * w as f64;
    let ey = oy + gt[5] * h as f64;

    let mut xs = [ox, ex, ox, ex];
    let mut ys = [oy, oy, ey, ey];

    let mut src_srs = SpatialRef::from_wkt(&ds.projection()).context("source SRS for bounds")?;
    src_srs.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
    let mut wgs84 = SpatialRef::from_epsg(4326).context("EPSG:4326")?;
    wgs84.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
    let to_wgs84 = CoordTransform::new(&src_srs, &wgs84).context("coord transform")?;
    to_wgs84
        .transform_coords(&mut xs, &mut ys, &mut [] as &mut [f64])
        .context("transform corners to WGS84")?;

    let west = xs.iter().cloned().fold(f64::MAX, f64::min);
    let east = xs.iter().cloned().fold(f64::MIN, f64::max);
    let south = ys.iter().cloned().fold(f64::MAX, f64::min);
    let north = ys.iter().cloned().fold(f64::MIN, f64::max);

    eprintln!(
        "Input: {}×{} px  WGS84 bounds [{:.4}W {:.4}S {:.4}E {:.4}N]",
        w, h, west, south, east, north
    );
    Ok((west, south, east, north))
}

// ── Per-thread dataset cache ──────────────────────────────────────────────────
struct DatasetCache {
    input_path: String,
    dataset: Dataset,
    gt: [f64; 6],
    src_w: usize,
    src_h: usize,
    nodata_base: f32,
    src_is_wgs84: bool,
    to_src: Option<CoordTransform>,
}

thread_local! {
    static TILE_CACHE: RefCell<Option<DatasetCache>> = RefCell::new(None);
}

fn init_dataset_cache(input_path: &str) -> Result<DatasetCache> {
    let dataset = Dataset::open(input_path).context("open dataset")?;

    let gt = dataset.geo_transform().context("geo_transform")?;
    let (src_w, src_h) = dataset.raster_size();

    let nodata_base = dataset
        .rasterband(1)
        .context("rasterband 1")?
        .no_data_value()
        .unwrap_or(-32_767.0) as f32;

    let projection = dataset.projection();

    let mut src_srs = SpatialRef::from_wkt(&projection).context("source SRS")?;
    src_srs.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);

    let src_is_wgs84 = src_srs.is_geographic()
        && src_srs.auth_name().as_deref() == Some("EPSG")
        && src_srs.auth_code().ok() == Some(4326);

    let to_src = if !src_is_wgs84 {
        let mut srs_3857 = SpatialRef::from_epsg(3857).context("EPSG:3857")?;
        srs_3857.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
        Some(CoordTransform::new(&srs_3857, &src_srs).context("coord transform 3857→src")?)
    } else {
        None
    };

    Ok(DatasetCache {
        input_path: input_path.to_owned(),
        dataset,
        gt,
        src_w,
        src_h,
        nodata_base,
        src_is_wgs84,
        to_src,
    })
}

// ── Helper: elevation filtering and encoding ─────────────────────────────────
/// If `elev` is below `zero_below`, encode it as elevation 0.
/// Otherwise delegate to the actual encoder.
fn encode_elev(
    elev: f32,
    encoding: Encoding,
    base_val: f64,
    interval: f64,
    round: u32,
    nodata: f32,
    zero_below: Option<f32>,
) -> [u8; 3] {
    let elev = if (elev - nodata).abs() < 0.5 || elev.is_nan() {
        elev
    } else if zero_below.map_or(false, |min| elev < min) {
        0.0
    } else {
        elev
    };
    match encoding {
        Encoding::Mapbox => encode_mapbox(elev, base_val, interval, round, nodata),
        Encoding::Terrarium => encode_terrarium(elev, nodata),
    }
}

fn track_uniform_color(c: [u8; 3], first_color: &mut Option<[u8; 3]>, all_same: &mut bool) {
    if !*all_same {
        return;
    }
    match *first_color {
        Some(first) if first != c => *all_same = false,
        Some(_) => {}
        None => *first_color = Some(c),
    }
}

pub(crate) fn encode_solid_tile(color: [u8; 3], format: TileFormat, compress: Option<u8>) -> Result<Vec<u8>> {
    let mut rgb = vec![0u8; GRID_PIXELS * 3];
    for px in rgb.chunks_exact_mut(3) {
        px.copy_from_slice(&color);
    }
    match format {
        TileFormat::Webp => crate::tile_format::webp::encode_tile(&rgb, compress),
        TileFormat::Png => crate::tile_format::png::encode_tile(&rgb, compress),
    }
}

/// Process one tile; returns `None` if entirely nodata.
/// Uses a per-thread dataset cache — GDAL datasets are not Send but are safe
/// to reuse on the same thread across tiles.
///
/// # Parameters
/// - `zero_below`: values below this threshold are encoded as elevation 0.
///                 For example, use `Some(-50.0)` to encode ocean depths below -50 m as 0.
pub fn process_tile(
    input_path: &str,
    z: u8,
    x: u32,
    y_xyz: u32,
    base_val: f64,
    interval: f64,
    round: u32,
    encoding: Encoding,
    format: TileFormat,
    compress: Option<u8>,
    nodata_override: Option<f32>,
    zero_below: Option<f32>,
    fill_uniform_descendants: bool,
    read_buffer_size: usize,
) -> Result<Option<ProcessedTile>> {
    use gdal::raster::ResampleAlg;

    TILE_CACHE.with(|cell| -> Result<Option<ProcessedTile>> {
        // Ensure this thread's cache is warm for the current input path.
        {
            let mut opt = cell.borrow_mut();
            if opt.as_ref().map_or(true, |c| c.input_path != input_path) {
                *opt = Some(init_dataset_cache(input_path)?);
            }
        }

        let cache_ref = cell.borrow();
        let cache = cache_ref.as_ref().unwrap();

        let nodata = nodata_override.unwrap_or(cache.nodata_base);
        let gt = cache.gt;
        let src_w = cache.src_w;
        let src_h = cache.src_h;
        let src_is_wgs84 = cache.src_is_wgs84;

        let band = cache.dataset.rasterband(1).context("rasterband 1")?;

        // ── Tile extent in 3857 (original 512‑px boundary) ───────────────────
        let [west_m, south_m, east_m, north_m] = tile_bounds_3857(z, x, y_xyz);

        // ── Compute pixel step size (for 512 px) and expand for 1‑px skirt ──
        let pw = (east_m - west_m) / (TILE_SIZE as f64);
        let ph = (north_m - south_m) / (TILE_SIZE as f64);

        // Expanded geographic bounds covering the skirt pixels
        let west_ext = west_m - pw * (SKIRT as f64);
        let east_ext = east_m + pw * (SKIRT as f64);
        let south_ext = south_m - ph * (SKIRT as f64);
        let north_ext = north_m + ph * (SKIRT as f64);

        // ── Transform tile corners + midpoints to source SRS for read window ─────────────
        let mid_x = (west_ext + east_ext) / 2.0;
        let mid_y = (south_ext + north_ext) / 2.0;
        let mut cx = [west_ext, east_ext, west_ext, east_ext, mid_x, west_ext, east_ext, mid_x];
        let mut cy = [south_ext, south_ext, north_ext, north_ext, mid_y, mid_y, mid_y, south_ext];
        if let Some(ref t) = cache.to_src {
            t.transform_coords(&mut cx, &mut cy, &mut [] as &mut [f64])
                .context("transform corners")?;
        } else {
            for i in 0..cx.len() {
                let (lon, lat) = merc_to_wgs84(cx[i], cy[i]);
                cx[i] = lon;
                cy[i] = lat;
            }
        }

        let src_x_min = cx.iter().cloned().fold(f64::MAX, f64::min);
        let src_x_max = cx.iter().cloned().fold(f64::MIN, f64::max);
        let src_y_min = cy.iter().cloned().fold(f64::MAX, f64::min);
        let src_y_max = cy.iter().cloned().fold(f64::MIN, f64::max);

        // ── Convert to pixel indices ─────────────────────────────────────────
        let px_min = (src_x_min - gt[0]) / gt[1];
        let px_max = (src_x_max - gt[0]) / gt[1];
        let py_min = (src_y_max - gt[3]) / gt[5];
        let py_max = (src_y_min - gt[3]) / gt[5];

        // Expand margin for bilinear interpolation; clamp to source bounds
        let rx0 = (px_min.floor() as i64 - 1).clamp(0, src_w as i64 - 1) as usize;
        let ry0 = (py_min.floor() as i64 - 1).clamp(0, src_h as i64 - 1) as usize;
        let rx1 = (px_max.ceil() as i64 + 2).clamp(rx0 as i64 + 1, src_w as i64) as usize;
        let ry1 = (py_max.ceil() as i64 + 2).clamp(ry0 as i64 + 1, src_h as i64) as usize;

        let rw = rx1 - rx0;
        let rh = ry1 - ry0;

        // Cap buffer size. Smaller values encourage GDAL to use overviews earlier.
        let bw = rw.min(read_buffer_size);
        let bh = rh.min(read_buffer_size);

        let buf = band
            .read_as::<f32>(
                (rx0 as isize, ry0 as isize),
                (rw, rh),
                (bw, bh),
                Some(ResampleAlg::Bilinear),
            )
            .context("read_as")?;
        let src_data = buf.data();

        let sx = bw as f64 / rw as f64; // source → buffer scale
        let sy = bh as f64 / rh as f64;

        // ── Early exit: if source buffer is entirely nodata, skip tile ─────
        let is_nodata_val = |v: f32| {
            (v - nodata).abs() < 0.5
                || v.is_nan()
        };
        if !src_data.iter().any(|&v| !is_nodata_val(v)) {
            return Ok(None);
        }

        // ── Build pixel coordinates and sample + encode ──────────────────────
        let mut rgb = vec![0u8; GRID_PIXELS * 3];
        let mut any_valid = false;
        let mut first_color = None;
        let mut all_same = fill_uniform_descendants;

        if src_is_wgs84 {
            // ── WGS84 fast path: separable lon/lat grid ──────────────────────
            let scale_x = sx / gt[1];
            let off_x = (gt[0] / gt[1] + rx0 as f64) * sx;
            let scale_y = sy / gt[5];
            let off_y = (gt[3] / gt[5] + ry0 as f64) * sy;

            let deg_per_merc = 180.0 / HALF_CIRC;
            let pi_over_hc = std::f64::consts::PI / HALF_CIRC;

            // Precompute per‑column: Mercator x → lon → buffer pixel x
            let mut bpx_col = [0.0f64; GRID_SIZE];
            for col in 0..GRID_SIZE {
                let x_m = west_ext + (col as f64 + 0.5) * pw;
                let lon = x_m * deg_per_merc;
                bpx_col[col] = lon * scale_x - off_x;
            }

            // Precompute per‑row: Mercator y → lat → buffer pixel y
            let mut bpy_row = [0.0f64; GRID_SIZE];
            for row in 0..GRID_SIZE {
                let y_m = north_ext - (row as f64 + 0.5) * ph;
                let lat = (2.0 * (y_m * pi_over_hc).exp().atan()
                    - std::f64::consts::FRAC_PI_2)
                    .to_degrees();
                bpy_row[row] = lat * scale_y - off_y;
            }

            // Fused sample + encode – no allocations, no per‑pixel trig
            for row in 0..GRID_SIZE {
                let bpy = bpy_row[row];
                let base = row * GRID_SIZE * 3;
                for col in 0..GRID_SIZE {
                    let elev = sample_bilinear(src_data, bw, bh, bpx_col[col], bpy, nodata);
                    let c = encode_elev(elev, encoding, base_val, interval, round, nodata, zero_below);
                    if c != [0, 0, 0] {
                        any_valid = true;
                    }
                    if fill_uniform_descendants {
                        track_uniform_color(c, &mut first_color, &mut all_same);
                    }
                    let idx = base + col * 3;
                    rgb[idx] = c[0];
                    rgb[idx + 1] = c[1];
                    rgb[idx + 2] = c[2];
                }
            }
        } else {
            // ── General path: full 262K+ coordinate transform ─────────────────
            let mut px3 = Vec::with_capacity(GRID_PIXELS);
            let mut py3 = Vec::with_capacity(GRID_PIXELS);
            for row in 0..GRID_SIZE {
                for col in 0..GRID_SIZE {
                    px3.push(west_ext + (col as f64 + 0.5) * pw);
                    py3.push(north_ext - (row as f64 + 0.5) * ph);
                }
            }
            cache
                .to_src
                .as_ref()
                .unwrap()
                .transform_coords(&mut px3, &mut py3, &mut [])
                .context("transform pixel grid")?;

            for i in 0..GRID_PIXELS {
                let bpx = ((px3[i] - gt[0]) / gt[1] - rx0 as f64) * sx;
                let bpy = ((py3[i] - gt[3]) / gt[5] - ry0 as f64) * sy;

                let elev = sample_bilinear(src_data, bw, bh, bpx, bpy, nodata);
                let c = encode_elev(elev, encoding, base_val, interval, round, nodata, zero_below);
                if c != [0, 0, 0] {
                    any_valid = true;
                }
                if fill_uniform_descendants {
                    track_uniform_color(c, &mut first_color, &mut all_same);
                }
                let start = i * 3;
                rgb[start..start + 3].copy_from_slice(&c);
            }
        }

        if !any_valid {
            return Ok(None);
        }

        let tile = match format {
            TileFormat::Webp => crate::tile_format::webp::encode_tile(&rgb, compress)?,
            TileFormat::Png => crate::tile_format::png::encode_tile(&rgb, compress)?,
        };
        let uniform_color = if all_same { first_color } else { None };
        Ok(Some(ProcessedTile { data: tile, uniform_color }))
    })
}
