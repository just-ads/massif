use std::collections::HashSet;
use std::fs;

use anyhow::{Context, Result};

use crate::container::mbtiles::MbtilesWriter;
use crate::frontier::{bounded_descendant_count, bounds_by_zoom, initial_frontier, tile_key};
use crate::pipeline::{append_children_unique_in_bounds, process_tiles_stream, TileOutcome, TileStats};
use crate::progress::SingleProgress;
use crate::Args;

pub(crate) fn run(
    args: &Args,
    input_str: &str,
    bounds: (f64, f64, f64, f64),
    upper_bound_tiles: usize,
) -> Result<()> {
    let (west_lon, south_lat, east_lon, north_lat) = bounds;
    let tile_bounds = bounds_by_zoom(west_lon, south_lat, east_lon, north_lat, args.min_z, args.max_z);
    let mut writer;
    let mut frontier;
    let mut start_zoom = args.min_z;
    let mut is_resume = false;

    if args.output.exists() {
        let existing = MbtilesWriter::open_or_create(&args.output, args.format, args.min_z, args.max_z)?;
        let status = existing.progress_status()?;
        match status.as_deref() {
            Some("processing_zoom") => {
                start_zoom = existing.current_zoom()?.context("MBTiles current_zoom missing")?;
                frontier = existing.load_frontier(start_zoom)?;
                writer = existing;
                is_resume = true;
                eprintln!(
                    "Resuming MBTiles generation at z{}: {} remaining frontier tiles",
                    start_zoom,
                    frontier.len()
                );
            }
            _ => {
                drop(existing);
                writer = MbtilesWriter::create(&args.output, args.format, args.min_z, args.max_z)?;
                frontier = initial_frontier(west_lon, south_lat, east_lon, north_lat, args.min_z);
                writer.save_frontier(args.min_z, &frontier)?;
                writer.set_progress("processing_zoom", args.min_z)?;
            }
        }
    } else {
        writer = MbtilesWriter::create(&args.output, args.format, args.min_z, args.max_z)?;
        frontier = initial_frontier(west_lon, south_lat, east_lon, north_lat, args.min_z);
        writer.save_frontier(args.min_z, &frontier)?;
        writer.set_progress("processing_zoom", args.min_z)?;
    }

    let mut total = TileStats::default();
    let chunk_size = 4096usize;
    let mut progress = SingleProgress::mbtiles(upper_bound_tiles as u64);

    for z in start_zoom..=args.max_z {
        writer.set_progress("processing_zoom", z)?;
        if !is_resume && frontier.is_empty() {
            frontier = writer.load_frontier(z)?;
        }
        is_resume = false;

        let mut next_frontier = if z < args.max_z { writer.load_frontier(z + 1)? } else { Vec::new() };
        let mut next_keys: HashSet<u64> = next_frontier.iter().copied().map(tile_key).collect();
        let mut zoom = TileStats::default();
        let mut processed = 0usize;
        progress.set_generate_stage(z, 0, frontier.len(), &total);

        while processed < frontier.len() {
            let end = (processed + chunk_size).min(frontier.len());
            let chunk = &frontier[processed..end];
            let existing = writer.existing_tiles(chunk)?;
            let mut work = Vec::new();

            for tile in chunk {
                if existing.contains(&tile_key(*tile)) {
                    append_children_unique_in_bounds(*tile, &mut next_frontier, &mut next_keys, args.max_z, &tile_bounds);
                    total.add_restored();
                    total.add_written();
                    total.add_checked();
                    zoom.add_restored();
                    zoom.add_written();
                    zoom.add_checked();
                    progress.advance_generation(1, z, zoom.checked, frontier.len(), &total);
                } else {
                    work.push(*tile);
                }
            }

            writer.begin_chunk()?;
            let chunk_result = process_tiles_stream(args, input_str, &work, |result| {
                    total.add_checked();
                    zoom.add_checked();

                    match result {
                        TileOutcome::Data { coord, data } => {
                            writer.add_tile(coord.z, coord.x, coord.y, &data).context("add MBTiles tile")?;
                            append_children_unique_in_bounds(coord, &mut next_frontier, &mut next_keys, args.max_z, &tile_bounds);
                            total.add_written();
                            zoom.add_written();
                            progress.advance_generation(1, z, zoom.checked, frontier.len(), &total);
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
            });

            if let Err(error) = chunk_result {
                let _ = writer.rollback_chunk();
                return Err(error);
            }
            writer.commit_chunk()?;

            processed = end;
            if z < args.max_z {
                writer.save_frontier(z + 1, &next_frontier)?;
            }
            writer.save_frontier(z, &frontier[processed..])?;
            writer.set_progress("processing_zoom", z)?;
        }

        writer.delete_frontier(z)?;
        if z < args.max_z {
            writer.save_frontier(z + 1, &next_frontier)?;
            writer.set_progress("processing_zoom", z + 1)?;
        }
        frontier = next_frontier;
    }

    progress.finish_generation(&total);
    progress.finish(&total);
    writer.finalize().context("finalize MBTiles")?;

    let sz = fs::metadata(&args.output)?.len();
    eprintln!("Written {:?}  ({:.1} MB)", args.output, sz as f64 / 1_048_576.0);
    Ok(())
}
