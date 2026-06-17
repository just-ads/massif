use std::collections::HashSet;
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::frontier::{
    append_children_in_bounds, bounded_descendant_count, bounds_by_zoom, initial_frontier, tile_key,
};
use crate::pipeline::{process_tiles_stream, TileOutcome, TileStats};
use crate::pmtiles::spool;
use crate::progress::SingleProgress;
use crate::raster::dataset_wgs84_bounds;
use crate::Args;

fn state_for(args: &Args, status: &str, current_zoom: u8) -> spool::ResumeState {
    spool::ResumeState {
        version: 1,
        format: "pmtiles".to_owned(),
        status: status.to_owned(),
        min_zoom: args.min_z,
        max_zoom: args.max_z,
        current_zoom,
        output: args.output.to_string_lossy().to_string(),
    }
}

fn validate_state(args: &Args, state: &spool::ResumeState, temp_root: &std::path::Path) -> Result<()> {
    if state.version != 1
        || state.format != "pmtiles"
        || state.min_zoom != args.min_z
        || state.max_zoom != args.max_z
        || state.output != args.output.to_string_lossy().as_ref()
    {
        bail!(
            "PMTiles temp state does not match current arguments: {:?}",
            spool::state_path(temp_root)
        );
    }
    Ok(())
}

pub(crate) fn build_from_temp(args: &Args) -> Result<()> {
    let temp_root = spool::temp_root(&args.output);
    if !temp_root.exists() {
        bail!("PMTiles temp tile pyramid does not exist: {:?}", temp_root);
    }
    if spool::state_path(&temp_root).exists() {
        let state = spool::read_state(&temp_root)?;
        validate_state(args, &state, &temp_root)?;
    }

    let (west_lon, south_lat, east_lon, north_lat) = dataset_wgs84_bounds(&args.input)?;
    let tile_bounds = bounds_by_zoom(west_lon, south_lat, east_lon, north_lat, args.min_z, args.max_z);

    let total = TileStats::default();
    spool::write_state(&temp_root, &state_for(args, "writing_pmtiles", args.max_z))?;
    let written = spool::build_pmtiles_from_temp(
        &temp_root,
        &args.output,
        args.format,
        args.compress,
        args.min_z,
        args.max_z,
        &tile_bounds,
        None,
        &total,
    )?;
    spool::write_state(&temp_root, &state_for(args, "done", args.max_z))?;

    let sz = fs::metadata(&args.output)?.len();
    eprintln!(
        "Written {:?}  ({:.1} MB, {} tiles)",
        args.output,
        sz as f64 / 1_048_576.0,
        written
    );
    Ok(())
}

#[allow(dead_code)]
pub(crate) fn regenerate_skipped_tiles(args: &Args, input_str: &str, log_path: Option<&Path>) -> Result<()> {
    let log_path = log_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| spool::skipped_tiles_path(&args.output));
    let tiles = spool::read_skipped_tiles_log(&log_path)?;
    if tiles.is_empty() {
        eprintln!("No skipped PMTiles tiles found in {:?}", log_path);
        return Ok(());
    }
    if let Some(tile) = tiles.iter().find(|tile| tile.z < args.min_z || tile.z > args.max_z) {
        bail!(
            "skipped tile {}/{}/{} is outside --min-z/--max-z ({}-{})",
            tile.z,
            tile.x,
            tile.y,
            args.min_z,
            args.max_z
        );
    }

    let temp_root = spool::temp_root(&args.output);
    if !temp_root.exists() {
        bail!(
            "PMTiles temp tile pyramid does not exist: {:?}; cannot rebuild the full output archive from only the skipped tile log",
            temp_root
        );
    }
    for z in args.min_z..=args.max_z {
        if tiles.iter().any(|tile| tile.z == z) {
            spool::prepare_zoom_write_dir(&temp_root, z)?;
        }
    }

    let chunk_size = 4096usize;
    let mut total = TileStats::default();
    eprintln!(
        "Regenerating {} skipped PMTiles tiles from {:?} into {:?}",
        tiles.len(),
        log_path,
        temp_root
    );

    for chunk in tiles.chunks(chunk_size) {
        process_tiles_stream(args, input_str, chunk, |result| {
            total.add_checked();
            match result {
                TileOutcome::Data { coord, data, .. } => {
                    spool::write_temp_tile(&temp_root, coord, &data)?;
                    total.add_written();
                }
                TileOutcome::Empty { coord } => {
                    eprintln!(
                        "Warning: regenerated skipped PMTiles tile {}/{}/{} is empty; not written",
                        coord.z, coord.x, coord.y
                    );
                    total.add_empty();
                }
                TileOutcome::Error { coord, error } => {
                    eprintln!(
                        "Warning: skipped PMTiles tile {}/{}/{} failed again: {:#}",
                        coord.z, coord.x, coord.y, error
                    );
                    total.add_error();
                }
            }
            Ok(())
        })?;
    }

    eprintln!(
        "Regenerated {} tiles into {:?} ({} empty, {} errors)",
        total.written, temp_root, total.empty, total.errors
    );

    let (west_lon, south_lat, east_lon, north_lat) = dataset_wgs84_bounds(&args.input)?;
    let tile_bounds = bounds_by_zoom(west_lon, south_lat, east_lon, north_lat, args.min_z, args.max_z);
    spool::write_state(&temp_root, &state_for(args, "writing_pmtiles", args.max_z))?;
    let written = spool::build_pmtiles_from_temp(
        &temp_root,
        &args.output,
        args.format,
        args.compress,
        args.min_z,
        args.max_z,
        &tile_bounds,
        None,
        &total,
    )?;
    spool::write_state(&temp_root, &state_for(args, "done", args.max_z))?;
    let skipped_tiles = spool::skipped_tiles_path(&args.output);
    if args.keep_temp || skipped_tiles.exists() {
        eprintln!("Kept PMTiles temp tile pyramid: {:?}", temp_root);
    } else {
        fs::remove_dir_all(&temp_root).with_context(|| format!("remove {:?}", temp_root))?;
    }

    let sz = fs::metadata(&args.output)?.len();
    eprintln!(
        "Written {:?}  ({:.1} MB, {} tiles)",
        args.output,
        sz as f64 / 1_048_576.0,
        written
    );
    Ok(())
}

pub(crate) fn run(
    args: &Args,
    input_str: &str,
    bounds: (f64, f64, f64, f64),
    upper_bound_tiles: usize,
) -> Result<()> {
    let (west_lon, south_lat, east_lon, north_lat) = bounds;
    let tile_bounds = bounds_by_zoom(west_lon, south_lat, east_lon, north_lat, args.min_z, args.max_z);
    let temp_root = spool::temp_root(&args.output);
    let mut frontier;
    let mut start_zoom = args.min_z;
    let mut need_restore = false;
    let mut restore_zoom = args.min_z;
    let mut existing_encoded: HashSet<u64> = HashSet::new();
    let mut build_pmtiles_only = false;

    if spool::state_path(&temp_root).exists() {
        let state = spool::read_state(&temp_root)?;
        validate_state(args, &state, &temp_root)?;

        match state.status.as_str() {
            "processing_zoom" => {
                start_zoom = state.current_zoom;
                restore_zoom = state.current_zoom;
                need_restore = true;
                let writing = spool::zoom_dir(&temp_root, restore_zoom).join(".writing");
                if writing.exists() {
                    fs::remove_dir_all(&writing).with_context(|| format!("remove {:?}", writing))?;
                }
                spool::remove_frontier_writing_files(&temp_root)?;
                let next = spool::frontier_path(&temp_root, restore_zoom.saturating_add(1));
                if next.exists() {
                    fs::remove_file(&next).with_context(|| format!("remove stale {:?}", next))?;
                }
                existing_encoded = spool::scan_existing_encoded(&temp_root, restore_zoom)?;
                frontier = spool::read_frontier(&temp_root, restore_zoom)?;
                eprintln!(
                    "Resuming PMTiles generation at z{}: {} existing non-empty tiles will be skipped",
                    restore_zoom,
                    existing_encoded.len()
                );
            }
            "all_tiles_done" => {
                build_pmtiles_only = true;
                frontier = Vec::new();
            }
            "writing_pmtiles" => {
                let partial = spool::partial_output(&args.output);
                if partial.exists() {
                    fs::remove_file(&partial).with_context(|| format!("remove {:?}", partial))?;
                }
                build_pmtiles_only = true;
                frontier = Vec::new();
            }
            "done" => {
                eprintln!("PMTiles state is already done: {:?}", spool::state_path(&temp_root));
                return Ok(());
            }
            other => bail!("unsupported PMTiles resume status: {}", other),
        }
    } else {
        if temp_root.exists() {
            fs::remove_dir_all(&temp_root).with_context(|| format!("remove stale {:?}", temp_root))?;
        }
        frontier = initial_frontier(west_lon, south_lat, east_lon, north_lat, args.min_z);
        spool::write_frontier(&temp_root, args.min_z, &frontier)?;
        spool::write_state(&temp_root, &state_for(args, "processing_zoom", args.min_z))?;
    }

    let mut total = TileStats::default();
    let chunk_size = 4096usize;
    let mut progress = SingleProgress::pmtiles(upper_bound_tiles as u64);
    let mut uniform_fill_roots = spool::uniform_fill_roots(&temp_root)?;

    if !build_pmtiles_only {
        for z in start_zoom..=args.max_z {
            spool::write_state(&temp_root, &state_for(args, "processing_zoom", z))?;
            spool::prepare_zoom_write_dir(&temp_root, z)?;

            let mut next_frontier = Vec::new();
            let mut zoom = TileStats::default();
            progress.set_generate_stage(z, 0, frontier.len(), &total);

            for chunk in frontier.chunks(chunk_size) {
                let mut work = Vec::new();

                for tile in chunk {
                    if need_restore && z == restore_zoom && existing_encoded.contains(&tile_key(*tile)) {
                        let filled = if uniform_fill_roots.contains(&tile_key(*tile)) {
                            bounded_descendant_count(*tile, &tile_bounds, args.max_z)
                        } else {
                            append_children_in_bounds(*tile, &mut next_frontier, args.max_z, &tile_bounds);
                            0
                        };
                        total.add_restored();
                        total.add_written_n(1 + filled);
                        total.add_filled(filled);
                        total.add_checked();
                        zoom.add_restored();
                        zoom.add_written_n(1 + filled);
                        zoom.add_filled(filled);
                        zoom.add_checked();
                        progress.advance_generation(1 + filled, z, zoom.checked, frontier.len(), &total);
                    } else {
                        work.push(*tile);
                    }
                }

                process_tiles_stream(args, input_str, &work, |result| {
                    total.add_checked();
                    zoom.add_checked();

                    match result {
                        TileOutcome::Data { coord, data, uniform_color } => {
                            spool::write_temp_tile(&temp_root, coord, &data)?;
                            if let Some(color) = uniform_color {
                                let filled = bounded_descendant_count(coord, &tile_bounds, args.max_z);
                                if filled > 0 {
                                    spool::append_uniform_fill(
                                        &temp_root,
                                        spool::UniformFill { root: coord, color },
                                    )?;
                                    uniform_fill_roots.insert(tile_key(coord));
                                }
                                total.add_written_n(1 + filled);
                                total.add_filled(filled);
                                zoom.add_written_n(1 + filled);
                                zoom.add_filled(filled);
                                progress.advance_generation(1 + filled, z, zoom.checked, frontier.len(), &total);
                            } else {
                                append_children_in_bounds(coord, &mut next_frontier, args.max_z, &tile_bounds);
                                total.add_written();
                                zoom.add_written();
                                progress.advance_generation(1, z, zoom.checked, frontier.len(), &total);
                            }
                        }
                        TileOutcome::Empty { coord } => {
                            let pruned = bounded_descendant_count(coord, &tile_bounds, args.max_z);
                            total.add_empty();
                            total.add_pruned(pruned);
                            zoom.add_empty();
                            zoom.add_pruned(pruned);
                            progress.advance_generation(1 + pruned, z, zoom.checked, frontier.len(), &total);
                        }
                        TileOutcome::Error { coord, error } => {
                            progress.write_warning(format!(
                                "Warning: tile {}/{}/{} failed: {:#}",
                                coord.z, coord.x, coord.y, error
                            ));
                            total.add_error();
                            zoom.add_error();
                            progress.advance_generation(1, z, zoom.checked, frontier.len(), &total);
                        }
                    }
                    Ok(())
                })?;
            }

            if need_restore && z == restore_zoom {
                existing_encoded.clear();
                need_restore = false;
            }

            if z < args.max_z {
                spool::write_frontier(&temp_root, z + 1, &next_frontier)?;
                spool::write_state(&temp_root, &state_for(args, "processing_zoom", z + 1))?;
            } else {
                spool::write_state(&temp_root, &state_for(args, "all_tiles_done", z))?;
            }
            let completed_frontier = spool::frontier_path(&temp_root, z);
            if completed_frontier.exists() {
                fs::remove_file(&completed_frontier)
                    .with_context(|| format!("remove completed {:?}", completed_frontier))?;
            }

            frontier = next_frontier;
        }
    }

    progress.finish_generation(&total);

    spool::write_state(&temp_root, &state_for(args, "writing_pmtiles", args.max_z))?;
    total.written = spool::build_pmtiles_from_temp(
        &temp_root,
        &args.output,
        args.format,
        args.compress,
        args.min_z,
        args.max_z,
        &tile_bounds,
        Some(&mut progress),
        &total,
    )?;
    progress.finish(&total);
    spool::write_state(&temp_root, &state_for(args, "done", args.max_z))?;
    let skipped_tiles = spool::skipped_tiles_path(&args.output);
    if args.keep_temp || skipped_tiles.exists() {
        eprintln!("Kept PMTiles temp tile pyramid: {:?}", temp_root);
    } else {
        fs::remove_dir_all(&temp_root).with_context(|| format!("remove {:?}", temp_root))?;
    }

    let sz = fs::metadata(&args.output)?.len();
    eprintln!("Written {:?}  ({:.1} MB)", args.output, sz as f64 / 1_048_576.0);
    Ok(())
}
