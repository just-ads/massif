use std::collections::HashSet;
use std::sync::mpsc::sync_channel;

use anyhow::Result;
use rayon::prelude::*;

use crate::frontier::{append_children_in_bounds, tile_key, TileBounds, TileJob};
use crate::raster::process_tile;
use crate::Args;

pub(crate) enum TileOutcome {
    Data { coord: TileJob, data: Vec<u8>, uniform_color: Option<[u8; 3]> },
    Empty { coord: TileJob },
    Error { coord: TileJob, error: anyhow::Error },
}

#[derive(Default)]
pub(crate) struct TileStats {
    pub(crate) checked: u64,
    pub(crate) written: u64,
    pub(crate) empty: u64,
    pub(crate) errors: u64,
    pub(crate) restored: u64,
    pub(crate) pruned: u64,
    pub(crate) filled: u64,
}

impl TileStats {
    pub(crate) fn add_checked(&mut self) {
        self.checked += 1;
    }

    pub(crate) fn add_written(&mut self) {
        self.written += 1;
    }

    pub(crate) fn add_written_n(&mut self, n: u64) {
        self.written += n;
    }

    pub(crate) fn add_empty(&mut self) {
        self.empty += 1;
    }

    pub(crate) fn add_error(&mut self) {
        self.errors += 1;
    }

    pub(crate) fn add_restored(&mut self) {
        self.restored += 1;
    }

    pub(crate) fn add_pruned(&mut self, n: u64) {
        self.pruned += n;
    }

    pub(crate) fn add_filled(&mut self, n: u64) {
        self.filled += n;
    }
}

pub(crate) fn process_tiles_stream(
    args: &Args,
    input_str: &str,
    work: &[TileJob],
    mut handle: impl FnMut(TileOutcome) -> Result<()> + Send,
) -> Result<()> {
    let (tx, rx) = sync_channel(128);

    rayon::scope(|scope| {
        scope.spawn(move |_| {
            work.par_iter().for_each_with(tx, |tx, tile| {
                let _ = tx.send(process_tile_job(args, input_str, *tile));
            });
        });

        let mut first_error = None;
        for outcome in rx {
            if first_error.is_some() {
                continue;
            }
            if let Err(error) = handle(outcome) {
                first_error = Some(error);
            }
        }
        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(())
        }
    })
}

fn process_tile_job(args: &Args, input_str: &str, tile: TileJob) -> TileOutcome {
    match process_tile(
        input_str,
        tile.z,
        tile.x,
        tile.y,
        args.base_val,
        args.interval,
        args.round_digits,
        args.encoding,
        args.format,
        args.compress,
        args.nodata,
        args.zero_below,
        args.fill_uniform_descendants,
        args.read_buffer_size,
    ) {
        Ok(Some(processed)) => TileOutcome::Data {
            coord: tile,
            data: processed.data,
            uniform_color: processed.uniform_color,
        },
        Ok(None) => TileOutcome::Empty { coord: tile },
        Err(error) => TileOutcome::Error { coord: tile, error },
    }
}

pub(crate) fn append_children_unique_in_bounds(
    tile: TileJob,
    next_frontier: &mut Vec<TileJob>,
    next_keys: &mut HashSet<u64>,
    max_z: u8,
    bounds: &[TileBounds],
) {
    let start = next_frontier.len();
    append_children_in_bounds(tile, next_frontier, max_z, bounds);
    let mut write = start;
    for read in start..next_frontier.len() {
        let child = next_frontier[read];
        if next_keys.insert(tile_key(child)) {
            next_frontier[write] = child;
            write += 1;
        }
    }
    next_frontier.truncate(write);
}
