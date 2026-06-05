# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/), and this project adheres to [Semantic Versioning](https://semver.org/).

## [1.0.0] - 2026-06-04

### Highlights

- Promote massif from the original full tile-list generator to a sparse frontier pipeline: generation starts at `--min-z`, expands only non-empty tiles, and prunes empty in-bounds descendants instead of processing every candidate tile from the initial bounds.
- Split PMTiles and MBTiles into dedicated generation flows. PMTiles now stages encoded tiles in `{output}.tmp` before a final sorted build, while MBTiles writes directly to SQLite.
- Add resumable generation. PMTiles resumes from `state.json`, `frontier_zN`, and staged tile files; MBTiles resumes from chunk-committed tiles plus temporary progress/frontier tables.
- Replace the original per-tile-list progress bar with a single staged progress bar that reports generation/build stage, written tiles, empty tiles, pruned descendants, and uniform-filled descendants.

### Added

- Add `--read-buffer-size` to choose 1024 or 2048 source read buffers per tile.
- Add `--zero-below` support for encoding values below a minimum elevation as 0.
- Add `--fill-uniform-descendants` to materialize descendants of uniform encoded tiles with a recorded RGB value. This is disabled by default because it is a lossy heuristic: parent-tile uniformity may hide deeper source variation.

### Performance

- Reduce unnecessary work versus the original `0.1.0` flow by pruning entire empty tile subtrees instead of generating every zoom/x/y candidate tile.
- Reduce peak chunk memory by streaming tile results through a bounded channel instead of collecting a whole chunk of encoded tile buffers before writing.
- Reduce PMTiles staging overhead by preparing each zoom write directory once rather than checking/creating it for every tile.
- Improve low-zoom IO control with `--read-buffer-size 1024`, which can encourage GDAL overview usage earlier and lower per-tile read memory.
- Keep MBTiles fast while adding resume support by committing chunk transactions and checking existing tiles in batches instead of querying per tile.
- Skip intermediate processing for uniform encoded tile subtrees when `--fill-uniform-descendants` is enabled; PMTiles records delayed fill rules, MBTiles fills descendants in the current transaction.

### Reliability

- Clip child tile expansion to the bounds-derived tile range at every zoom, so sparse frontier expansion does not drift outside the requested input bounds.
- Use atomic `.writing` files for PMTiles state/frontier/temp-tile writes and clean stale frontier `.writing` files on resume.
- Save MBTiles next-frontier state before current-frontier progress to prefer safe reprocessing after crashes.
- Drop temporary MBTiles progress/frontier tables during finalize so completed MBTiles files do not retain massif helper metadata.
- Keep `--zero-below` from converting nodata/NaN samples into elevation 0.
- Reject `--max-z > 31` because tile coordinates are stored as `u32`; descendant fill also uses checked shifts to avoid overflow.

## [0.1.1] - 2026-03-30

### Performance

- Exploit separable Mercator→WGS84 projection for EPSG:4326 inputs: 512+512 coordinate conversions per tile instead of 262,144
- Precompute geo→buffer-pixel mapping per row/col, removing all division and trig from the inner sampling loop
- Eliminate two 2MB Vec allocations per tile on the WGS84 fast path (stack arrays instead)
- Early nodata scan on source buffer skips empty edge tiles before any coordinate work
- Cache GDAL dataset per rayon thread — each worker opens the file once instead of per tile (major win for VRT inputs)
- Skip GDAL coordinate transforms entirely for EPSG:4326 inputs (direct inline math)
- Skip Hilbert sort for MBTiles output (SQLite insertion order doesn't matter)
- Use `sort_by_cached_key` for PMTiles Hilbert sort — computes tile IDs once instead of per comparison

## [0.1.0] - 2026-03-29

Initial release.

### Features

- Convert elevation rasters (GeoTIFF, VRT, any GDAL format, any pixel type) to terrain-RGB tiles
- Mapbox and Terrarium encoding schemes
- WebP and PNG tile formats
- PMTiles v3 and MBTiles output containers (inferred from file extension)
- Dual WebP encoder: fast pure-Rust path (no `--compress`) and libwebp path (`--compress 1-9`)
- Parallel processing via rayon with configurable thread count
- Real-time progress bar with tiles/sec and ETA
- Hilbert-sorted tile output for optimal PMTiles performance
- Chunked processing to bound peak memory usage
- Bilinear resampling with nodata-aware fallback
- Configurable Mapbox encoding parameters (`--base-val`, `--interval`, `--round-digits`)
- Nodata override (`--nodata`) for rasters with missing or incorrect metadata
- Any input CRS — automatic reprojection to Web Mercator via GDAL

[1.0.0]: https://github.com/mapriot/massif/releases/tag/v1.0.0
[0.1.1]: https://github.com/mapriot/massif/releases/tag/v0.1.1
[0.1.0]: https://github.com/mapriot/massif/releases/tag/v0.1.0
