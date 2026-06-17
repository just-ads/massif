# massif

Fast terrain-RGB tile generator from elevation rasters.

Converts GeoTIFF, VRT, or any GDAL-supported elevation raster into Mapbox or Terrarium terrain-RGB tiles, packaged as [PMTiles](https://protomaps.com/docs/pmtiles) or [MBTiles](https://wiki.openstreetmap.org/wiki/MBTiles). Ready to use with MapLibre GL for hillshading and 3D terrain.

Built as a fast Rust replacement for [rio-rgbify](https://github.com/mapbox/rio-rgbify). Uses all CPU cores via [rayon](https://github.com/rayon-rs/rayon), prunes empty tile subtrees with a sparse frontier, shows real-time progress, and outputs to modern tile containers — no Python overhead, no guessing when it'll finish.

## Installation

### Prerequisites

GDAL must be installed on your system.

| Platform | Command |
|---|---|
| macOS | `brew install gdal` |
| Ubuntu / Debian | `sudo apt install libgdal-dev gdal-bin` |
| Fedora / RHEL | `sudo dnf install gdal-devel` |
| Windows | [OSGeo4W](https://trac.osgeo.org/osgeo4w/) or [Conda](https://anaconda.org/conda-forge/gdal) — ensure `gdal-config` is on your PATH *(untested)* |

### Install massif

**From crates.io**
```bash
cargo install massif
```

**From source**
```bash
git clone https://github.com/mapriot/massif
cd massif
cargo build --release
# Binary is at target/release/massif
```

On macOS with Homebrew GDAL you may need:
```bash
PKG_CONFIG_PATH="/opt/homebrew/lib/pkgconfig" cargo build --release
```

## Usage

```
massif [OPTIONS] <INPUT> <OUTPUT>
```

`INPUT` is any GDAL-supported elevation raster (GeoTIFF, VRT, HGT, etc., any CRS).
`OUTPUT` is `.pmtiles` or `.mbtiles` — the container format is inferred from the extension.

### Quick start

```bash
# Fastest — preview / iteration (WebP, no extra compression)
massif input.tif output.pmtiles

# Production — good balance of size and speed
massif --compress 6 input.tif output.pmtiles

# MBTiles output — same flags, different extension
massif --compress 6 input.tif output.mbtiles

# Limit CPU usage on shared machines
massif --workers 4 input.tif output.pmtiles

# Encourage GDAL overview usage and reduce per-tile read memory
massif --read-buffer-size 1024 input.tif output.pmtiles

# Optional lossy optimization for large uniform encoded regions
massif --fill-uniform-descendants input.tif output.pmtiles

# Terrarium encoding
massif --encoding terrarium --compress 6 input.tif output.pmtiles

# PNG tiles
massif --format png --compress 6 input.tif output.pmtiles

# Maximum compression for smallest files (diminishing returns past r=5)
massif --compress 6 -r 5 input.tif output.pmtiles
```

### All options

| Flag | Default | Description |
|---|---|---|
| `--encoding` | `mapbox` | RGB encoding: `mapbox` or `terrarium` |
| `--format` | `webp` | Tile image format: `webp` or `png` |
| `--compress` | *(omitted)* | Compression effort 1–9; omit for fastest |
| `--min-z` | `5` | Minimum zoom level |
| `--max-z` | `12` | Maximum zoom level |
| `--nodata` | *(from raster)* | Override nodata value (e.g. `0`, `-9999`, `-32768`) |
| `--zero-below` | *(omitted)* | Encode values below this elevation as 0 |
| `--fill-uniform-descendants` | `false` | Fill descendants of uniform encoded tiles with the same RGB value; lossy heuristic |
| `-j, --workers` | all CPUs | Thread count |
| `--read-buffer-size` | `2048` | Maximum GDAL source read buffer per tile: `1024` or `2048` |
| `--keep-temp` | `false` | PMTiles only: keep `{output}.tmp` tile pyramid after writing the archive |
| `--build-from-temp` | `false` | PMTiles only: skip generation and build the archive from existing `{output}.tmp` |

**Mapbox encoding only:**

| Flag | Default | Description |
|---|---|---|
| `-b, --base-val` | `-10000` | Base elevation offset |
| `-i, --interval` | `0.1` | Elevation precision in metres |
| `-r, --round-digits` | `3` | Zero out lowest N bits of encoded value (reduces entropy) |

## Input preparation

**GDAL overviews** precompute downsampled versions of your raster so massif can read low-zoom tiles cheaply instead of resampling the full-resolution data each time. This reduces processing time by 20–40%.

```bash
# Single TIF — writes a sidecar .ovr file, does not modify the input
gdaladdo -ro -r average input.tif 2 4 8 16 32 64 128 256

# VRT — same approach, creates merged.vrt.ovr
gdaladdo -ro -r average merged.vrt 2 4 8 16 32 64 128 256
```

Massif (via GDAL) picks up the `.ovr` sidecar automatically. Overview selection depends on the source window size and the requested read buffer size. `--read-buffer-size 1024` usually encourages GDAL to use overviews earlier and reduces per-tile IO/memory; `2048` is more conservative and is the default.

The tradeoff is storage: a full overview pyramid can add roughly one third of the source size when compressed, and more if uncompressed. If disk space is constrained, skip overviews and run without — massif handles it, just slower.

## Progress, pruning, and resume

Massif builds tiles with a sparse frontier instead of precomputing every tile. It starts from `--min-z`; non-empty tiles expand to children, while empty tiles prune their entire in-bounds descendant subtree. This avoids spending time and memory on large nodata regions.

The progress bar uses the bounds-derived candidate tile count as the denominator. A checked non-empty tile advances by 1. An empty tile advances by `1 + pruned descendants`, so large empty areas are reflected immediately in global progress. For PMTiles, generation uses 90% of the bar and the final PMTiles build uses the remaining 10%.

`--fill-uniform-descendants` is an optional lossy optimization for datasets with large regions whose encoded RGB value is constant, such as flat ocean or masked areas. When a processed tile's full encoded 514×514 RGB grid is identical, massif records that RGB value and fills every in-bounds descendant tile with a pure solid tile instead of sampling and encoding the descendants. This can skip large uniform subtrees, but it assumes parent-tile uniformity is acceptable for deeper zooms. Leave it disabled if hidden higher-zoom variation must be preserved.

Uniform fill is handled differently per output format:

- PMTiles writes the current tile to `{output}.tmp`, records a delayed fill rule in `{output}.tmp/uniform_fills`, and materializes the filled descendants during the final PMTiles build. This avoids exploding the temporary tile pyramid.
- MBTiles writes the current tile and all filled descendants immediately in the same SQLite chunk transaction. If the transaction fails, both the root tile and its filled descendants roll back together.

PMTiles and MBTiles support interrupted runs differently:

- PMTiles writes encoded tiles to `{output}.tmp/zN/z_x_y` first, keeps `state.json` and `frontier_zN` files, then builds the final `.pmtiles` after all zooms finish. If interrupted, rerun the same command to resume from the temp directory.
- MBTiles writes directly to SQLite in chunk transactions. During processing it keeps temporary `massif_progress` and `massif_frontier` tables for resume. These helper tables are dropped during finalization, so completed MBTiles files do not retain massif metadata.

With `--fill-uniform-descendants`, resume remains output-correct. PMTiles persists fill rules and skips child expansion for restored uniform roots when the rule exists; if a crash happens between writing a root tile and recording its fill rule, massif safely falls back to normal child processing for that subtree. MBTiles commits uniform roots and descendants atomically, but resumed runs may still enqueue already-filled descendants for existing-tile checks because MBTiles does not currently persist uniform-root metadata; this affects resume speed and intermediate statistics, not tile correctness.

For PMTiles, use `--keep-temp` to retain the completed `{output}.tmp` tile pyramid after the archive is written. Use `--build-from-temp` to rebuild the `.pmtiles` archive from an existing `{output}.tmp` without regenerating tiles. If final PMTiles writing skipped unreadable or empty temp tiles, massif records them in `{output}.skipped_pmtiles_tiles.log` and keeps `{output}.tmp` for repair; after fixing the source/read issue, use the separate `massif-repair-pmtiles` CLI to regenerate only those tiles into the existing temp pyramid and rebuild the final `.pmtiles` in the same run. Delete `{output}.tmp` to force a clean restart.

### Repair skipped PMTiles tiles

`massif-repair-pmtiles` is a separate CLI entry point for repairing skipped PMTiles temp tiles and rebuilding the final archive:

```bash
massif-repair-pmtiles input.tif output.pmtiles --keep-temp
```

By default it reads `{output}.skipped_pmtiles_tiles.log`. Use `--skipped-tiles-log PATH` to override the log path. The encoding, zoom, compression, nodata, and read-buffer options should match the original `massif` run. Build it as a standalone executable with:

```bash
cargo build --release --bin massif-repair-pmtiles
```

## Performance

### Single large TIF — 7.2 GB (Indonesia, zoom 5–12, ~142K tiles)

| Machine | Version | Overviews | Command | Time | Output |
|---|---|---|---|---|---|
| **Apple M4 Pro, 14 threads** | **v0.1.1** | **yes** | **`massif`** | **0:51** | **4,560 MB** |
| **Apple M4 Pro, 14 threads** | **v0.1.1** | **yes** | **`massif --compress 6`** | **4:52** | **2,844 MB** |
| **Apple M4 Pro, 14 threads** | **v0.1.1** | **no** | **`massif`** | **2:02** | **4,560 MB** |
| **Apple M4 Pro, 14 threads** | **v0.1.1** | **no** | **`massif --compress 6`** | **6:18** | **2,844 MB** |
| Apple M4 Pro, 14 threads | v0.1.0 | no | `massif` | 2:30 | 4,560 MB |
| Apple M4 Pro, 14 threads | v0.1.0 | yes | `massif` | 1:28 | 4,560 MB |
| Apple M4 Pro, 14 threads | v0.1.0 | no | `massif --compress 6` | 6:29 | 2,844 MB |
| Apple M4 Pro, 14 threads | v0.1.0 | yes | `massif --compress 6` | 5:35 | 2,844 MB |
| Xeon Silver 4210, 20 threads | v0.1.0 | no | `massif` | 7:20 | 4,560 MB |
| Xeon Silver 4210, 20 threads | v0.1.0 | yes | `massif` | 5:42 | 4,560 MB |
| Xeon Silver 4210, 20 threads | v0.1.0 | no | `massif --compress 6` | 16:21 | 2,844 MB |
| Xeon Silver 4210, 20 threads | v0.1.0 | yes | `massif --compress 6` | 12:44 | 2,844 MB |
| Xeon Silver 4210, 20 threads | — | no | `rio-rgbify` | 25:51 | ~2,810 MB |

### VRT of 70 TIFs — 66 GB total (Europe + Oceania, zoom 5–12)

| Machine | Command | Version |Time | Output |
|---|---|---|---|---|
| Xeon Silver 4210, 20 threads | `massif` | v0.1.1 |**1h 36m** | 48,062 MB |
| Xeon Silver 4210, 20 threads | `massif --compress 6` | v0.1.1 |**4h 00m** | 29,877 MB |
| Xeon Silver 4210, 20 threads | `massif` | v0.1.0 |**15h 47m** | 48,062 MB |
| Xeon Silver 4210, 20 threads | `rio-rgbify` | - | DNF after 48h | — |

rio-rgbify did not finish after 48 hours on the same machine and dataset. All massif tiles are 512×512 lossless WebP images. The Xeon results were measured on a server under normal production load — actual times on an idle machine would be lower.

| Setting | Impact | Notes |
|---|---|---|
| EPSG:4326 input | **~2.5× faster** (no compress) | massif skips GDAL transforms entirely; use `gdalwarp -t_srs EPSG:4326` |
| GDAL overviews | **−20–40%** time | Effective for single TIFs; `.ovr` can match source file size |
| WebP vs PNG | WebP is **2× smaller** | Use PNG only if client doesn't support WebP |
| `--compress 6` | **−38%** size vs no compression | Best size/speed tradeoff; gains flatten past 5 |
| `-r 3` (default) | **−43%** size vs r=0 | Biggest lever for file size; invisible for hillshading at most latitudes |
| Terrarium vs Mapbox | Terrarium is **3.1× larger** | No round-digits equivalent; use Mapbox when possible |

For full benchmark methodology, all 36 parameter combinations, and recommended settings by use case, see [docs/benchmarks.md](docs/benchmarks.md).

## Encoding schemes

### Mapbox (default)

```
encoded = floor((elevation - base_val) / interval)
R = (encoded >> 16) & 0xFF
G = (encoded >> 8)  & 0xFF
B =  encoded        & 0xFF
```

MapLibre decodes as:
```
height = base_val + (R × 65536 + G × 256 + B) × interval
```

With the defaults (`-b -10000 -i 0.1`), the encodable range is −10,000 m to +1,677,721.5 m at 0.1 m precision. The `-r` flag zeroes the lowest N bits of the encoded integer — this reduces entropy for better compression with negligible quality loss for hillshading. Note: `-r 3` may produce visible artifacts at high latitudes (e.g. northern Norway, Svalbard, Greenland) where elevation gradients are subtle; use `-r 1` or `-r 0` for polar regions.

### Terrarium

```
val = elevation + 32768
R = floor(val / 256)
G = floor(val) mod 256
B = floor(frac(val) × 256)
```

MapLibre decodes as:
```
height = (R × 256 + G + B / 256) − 32768
```

Range: −32,768 m to +32,767.996 m at ~0.004 m precision. Used by Mapzen and many open elevation datasets. No configurable parameters — `-b`, `-i`, and `-r` are ignored with a warning.

## Using with MapLibre GL

```json
{
  "sources": {
    "terrain": {
      "type": "raster-dem",
      "url": "pmtiles://https://example.com/terrain.pmtiles",
      "encoding": "mapbox",
      "tileSize": 512
    }
  },
  "terrain": {
    "source": "terrain",
    "exaggeration": 1.5
  },
  "layers": [
    {
      "id": "hillshading",
      "type": "hillshade",
      "source": "terrain"
    }
  ]
}
```

For Terrarium output, set `"encoding": "terrarium"` in the source.

## Input formats

Any raster supported by GDAL — GeoTIFF (`.tif`), Virtual Raster (`.vrt`), HGT, IMG, and more. Any pixel data type works (Float32, Float64, Int16, UInt16, etc.) — GDAL converts to Float32 internally. The input can be in any CRS; massif reprojects each tile to Web Mercator on the fly.

Common elevation data sources:
- [ALOS World 3D](https://www.eorc.jaxa.jp/ALOS/en/dataset/aw3d30/aw3d30_e.htm)
- [SRTM](https://www.usgs.gov/centers/eros/science/usgs-eros-archive-digital-elevation-shuttle-radar-topography-mission-srtm)
- [Copernicus DEM](https://dataspace.copernicus.eu/explore-data/data-collections/copernicus-contributing-missions/collections-description/COP-DEM) (GLO-30, GLO-90)

## License

MIT — see [LICENSE](LICENSE)
