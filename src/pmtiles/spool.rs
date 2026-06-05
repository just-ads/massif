use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::container::Writer;
use crate::frontier::{
    bounded_descendants, parse_tile_filename, pmtiles_tile_id, tile_filename, tile_key, TileBounds, TileJob,
};
use crate::pipeline::TileStats;
use crate::progress::SingleProgress;
use crate::raster::encode_solid_tile;
use crate::tile_format::TileFormat;

#[derive(Debug, Serialize, Deserialize)]
pub struct ResumeState {
    pub version: u8,
    pub format: String,
    pub status: String,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub current_zoom: u8,
    pub output: String,
}

#[derive(Clone, Copy, Debug)]
pub struct UniformFill {
    pub root: TileJob,
    pub color: [u8; 3],
}

pub fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut raw: OsString = path.as_os_str().to_owned();
    raw.push(suffix);
    PathBuf::from(raw)
}

pub fn temp_root(output: &Path) -> PathBuf {
    append_suffix(output, ".tmp")
}

pub fn partial_output(output: &Path) -> PathBuf {
    append_suffix(output, ".tmp.pmtiles")
}

pub fn state_path(root: &Path) -> PathBuf {
    root.join("state.json")
}

pub fn frontier_path(root: &Path, z: u8) -> PathBuf {
    root.join(format!("frontier_z{}", z))
}

pub fn uniform_fills_path(root: &Path) -> PathBuf {
    root.join("uniform_fills")
}

pub fn zoom_dir(root: &Path, z: u8) -> PathBuf {
    root.join(format!("z{}", z))
}

pub fn prepare_zoom_write_dir(root: &Path, z: u8) -> Result<()> {
    let writing = zoom_dir(root, z).join(".writing");
    fs::create_dir_all(&writing).with_context(|| format!("create {:?}", writing))
}

pub fn remove_frontier_writing_files(root: &Path) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(root).with_context(|| format!("scan {:?}", root))? {
        let entry = entry.context("read PMTiles temp entry")?;
        if !entry.file_type().context("PMTiles temp entry file type")?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with("frontier_z") && name.ends_with(".writing") {
            let path = entry.path();
            fs::remove_file(&path).with_context(|| format!("remove stale {:?}", path))?;
        }
    }
    Ok(())
}

pub fn write_state(root: &Path, state: &ResumeState) -> Result<()> {
    fs::create_dir_all(root).with_context(|| format!("create temp dir {:?}", root))?;
    let data = serde_json::to_vec_pretty(state).context("serialize state")?;
    let tmp = append_suffix(&state_path(root), ".writing");
    let mut file = File::create(&tmp).with_context(|| format!("create {:?}", tmp))?;
    file.write_all(&data).context("write state.json")?;
    file.flush().context("flush state.json")?;
    fs::rename(&tmp, state_path(root)).context("rename state.json")
}

pub fn read_state(root: &Path) -> Result<ResumeState> {
    let data = fs::read(state_path(root)).context("read state.json")?;
    serde_json::from_slice(&data).context("parse state.json")
}

pub fn write_frontier(root: &Path, z: u8, frontier: &[TileJob]) -> Result<()> {
    fs::create_dir_all(root).with_context(|| format!("create temp dir {:?}", root))?;
    let tmp = append_suffix(&frontier_path(root, z), ".writing");
    let mut file = File::create(&tmp).with_context(|| format!("create {:?}", tmp))?;
    for tile in frontier {
        writeln!(file, "{} {} {}", tile.z, tile.x, tile.y).context("write frontier")?;
    }
    file.flush().context("flush frontier")?;
    fs::rename(&tmp, frontier_path(root, z)).context("rename frontier")
}

pub fn read_frontier(root: &Path, z: u8) -> Result<Vec<TileJob>> {
    let path = frontier_path(root, z);
    let file = File::open(&path).with_context(|| format!("open {:?}", path))?;
    let mut frontier = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line.context("read frontier line")?;
        let mut parts = line.split_whitespace();
        let z = parts.next().context("missing z")?.parse().context("parse z")?;
        let x = parts.next().context("missing x")?.parse().context("parse x")?;
        let y = parts.next().context("missing y")?.parse().context("parse y")?;
        frontier.push(TileJob { z, x, y });
    }
    Ok(frontier)
}

pub fn append_uniform_fill(root: &Path, fill: UniformFill) -> Result<()> {
    fs::create_dir_all(root).with_context(|| format!("create temp dir {:?}", root))?;
    let path = uniform_fills_path(root);
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {:?}", path))?;
    writeln!(
        file,
        "{} {} {} {} {} {}",
        fill.root.z, fill.root.x, fill.root.y, fill.color[0], fill.color[1], fill.color[2]
    )
    .context("write uniform fill")?;
    file.flush().context("flush uniform fill")
}

pub fn read_uniform_fills(root: &Path) -> Result<Vec<UniformFill>> {
    let path = uniform_fills_path(root);
    if !path.exists() {
        return Ok(Vec::new());
    }

    let file = File::open(&path).with_context(|| format!("open {:?}", path))?;
    let mut fills = Vec::new();
    let mut seen = HashSet::new();
    for line in BufReader::new(file).lines() {
        let line = line.context("read uniform fill line")?;
        let mut parts = line.split_whitespace();
        let z = parts.next().context("missing fill z")?.parse().context("parse fill z")?;
        let x = parts.next().context("missing fill x")?.parse().context("parse fill x")?;
        let y = parts.next().context("missing fill y")?.parse().context("parse fill y")?;
        let r = parts.next().context("missing fill r")?.parse().context("parse fill r")?;
        let g = parts.next().context("missing fill g")?.parse().context("parse fill g")?;
        let b = parts.next().context("missing fill b")?.parse().context("parse fill b")?;
        let root = TileJob { z, x, y };
        if seen.insert(tile_key(root)) {
            fills.push(UniformFill { root, color: [r, g, b] });
        }
    }
    Ok(fills)
}

pub fn uniform_fill_roots(root: &Path) -> Result<HashSet<u64>> {
    Ok(read_uniform_fills(root)?.into_iter().map(|fill| tile_key(fill.root)).collect())
}

pub fn write_temp_tile(root: &Path, tile: TileJob, data: &[u8]) -> Result<()> {
    if data.is_empty() {
        bail!("refusing to write empty temp tile {}/{}/{}", tile.z, tile.x, tile.y);
    }
    let dir = zoom_dir(root, tile.z);
    let writing = dir.join(".writing");
    let name = tile_filename(tile);
    let tmp = writing.join(&name);
    let final_path = dir.join(&name);
    fs::write(&tmp, data).with_context(|| format!("write {:?}", tmp))?;
    fs::rename(&tmp, &final_path).with_context(|| format!("rename {:?}", final_path))
}

pub fn scan_existing_encoded(root: &Path, z: u8) -> Result<HashSet<u64>> {
    let dir = zoom_dir(root, z);
    let mut existing = HashSet::new();
    if !dir.exists() {
        return Ok(existing);
    }
    for entry in fs::read_dir(&dir).with_context(|| format!("scan {:?}", dir))? {
        let entry = entry.context("read temp tile entry")?;
        if !entry.file_type().context("temp tile file type")?.is_file() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            if let Some(tile) = parse_tile_filename(name) {
                existing.insert(tile_key(tile));
            }
        }
    }
    Ok(existing)
}

fn collect_zoom_temp_files(root: &Path, z: u8) -> Result<Vec<(u64, TileJob, PathBuf)>> {
    let dir = zoom_dir(root, z);
    let mut files = Vec::new();
    if !dir.exists() {
        return Ok(files);
    }
    for entry in fs::read_dir(&dir).with_context(|| format!("scan {:?}", dir))? {
        let entry = entry.context("read temp tile entry")?;
        if !entry.file_type().context("temp tile file type")?.is_file() {
            continue;
        }
        let Some(tile) = entry.file_name().to_str().and_then(parse_tile_filename) else {
            continue;
        };
        files.push((pmtiles_tile_id(tile)?, tile, entry.path()));
    }
    files.sort_by_key(|(tile_id, _, _)| *tile_id);
    Ok(files)
}

fn count_pmtiles_temp_tiles(root: &Path, min_z: u8, max_z: u8) -> Result<u64> {
    let mut count = 0u64;
    for z in min_z..=max_z {
        count += collect_zoom_temp_files(root, z)?.len() as u64;
    }
    Ok(count)
}

fn fill_descendants_by_zoom(
    fills: &[UniformFill],
    bounds: &[TileBounds],
    max_z: u8,
) -> Result<HashMap<u8, Vec<(u64, TileJob, [u8; 3])>>> {
    let mut by_zoom: HashMap<u8, Vec<(u64, TileJob, [u8; 3])>> = HashMap::new();
    let mut seen = HashSet::new();
    for fill in fills {
        for tile in bounded_descendants(fill.root, bounds, max_z) {
            let key = tile_key(tile);
            if !seen.insert(key) {
                continue;
            }
            by_zoom
                .entry(tile.z)
                .or_default()
                .push((pmtiles_tile_id(tile)?, tile, fill.color));
        }
    }
    for entries in by_zoom.values_mut() {
        entries.sort_by_key(|(tile_id, _, _)| *tile_id);
    }
    Ok(by_zoom)
}

pub fn build_pmtiles_from_temp(
    root: &Path,
    output: &Path,
    format: TileFormat,
    compress: Option<u8>,
    min_z: u8,
    max_z: u8,
    bounds: &[TileBounds],
    mut progress: Option<&mut SingleProgress>,
    stats: &TileStats,
) -> Result<u64> {
    let partial = partial_output(output);
    if partial.exists() {
        fs::remove_file(&partial).with_context(|| format!("remove existing {:?}", partial))?;
    }
    let mut writer = Writer::open(&partial, format, min_z, max_z)?;
    let mut n_written = 0;
    let fills = read_uniform_fills(root)?;
    let fill_entries_by_zoom = fill_descendants_by_zoom(&fills, bounds, max_z)?;
    let total_temp_tiles = count_pmtiles_temp_tiles(root, min_z, max_z)?;
    let total_fill_tiles: u64 = fill_entries_by_zoom.values().map(|entries| entries.len() as u64).sum();
    if let Some(progress) = progress.as_deref_mut() {
        progress.start_build(total_temp_tiles + total_fill_tiles, stats);
    }
    let mut solid_cache: HashMap<[u8; 3], Vec<u8>> = HashMap::new();
    for z in min_z..=max_z {
        let files = collect_zoom_temp_files(root, z)?;
        let skip_keys: HashSet<u64> = files.iter().map(|(_, tile, _)| tile_key(*tile)).collect();
        let fill_entries = fill_entries_by_zoom
            .get(&z)
            .map(|entries| {
                entries
                    .iter()
                    .copied()
                    .filter(|(_, tile, _)| !skip_keys.contains(&tile_key(*tile)))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if files.is_empty() && fill_entries.is_empty() {
            continue;
        }
        let mut file_i = 0usize;
        let mut fill_i = 0usize;
        while file_i < files.len() || fill_i < fill_entries.len() {
            let take_file = if fill_i >= fill_entries.len() {
                true
            } else if file_i >= files.len() {
                false
            } else {
                files[file_i].0 <= fill_entries[fill_i].0
            };

            if take_file {
                let (_, tile, path) = &files[file_i];
                let data = fs::read(path).with_context(|| format!("read {:?}", path))?;
                if data.is_empty() {
                    bail!(
                        "refusing to add empty PMTiles tile {}/{}/{} from {:?}",
                        tile.z,
                        tile.x,
                        tile.y,
                        path
                    );
                }
                writer
                    .add_tile(tile.z, tile.x, tile.y, &data)
                    .context("add PMTiles tile")?;
                file_i += 1;
            } else {
                let (_, tile, color) = fill_entries[fill_i];
                let data = if let Some(data) = solid_cache.get(&color) {
                    data
                } else {
                    let data = encode_solid_tile(color, format, compress)?;
                    solid_cache.insert(color, data);
                    solid_cache.get(&color).unwrap()
                };
                writer
                    .add_tile(tile.z, tile.x, tile.y, data)
                    .context("add PMTiles uniform fill tile")?;
                fill_i += 1;
            }

            n_written += 1;
            if let Some(progress) = progress.as_deref_mut() {
                progress.advance_build(1, stats);
            }
        }
    }
    writer.finalize().context("finalize PMTiles")?;
    if output.exists() {
        fs::remove_file(output).with_context(|| format!("remove existing {:?}", output))?;
    }
    fs::rename(&partial, output).with_context(|| format!("rename {:?} to {:?}", partial, output))?;
    Ok(n_written)
}
