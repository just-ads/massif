use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::container::Writer;
use crate::frontier::{parse_tile_filename, pmtiles_tile_id, tile_filename, tile_key, TileJob};
use crate::pipeline::TileStats;
use crate::progress::SingleProgress;
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

pub fn write_temp_tile(root: &Path, tile: TileJob, data: &[u8]) -> Result<()> {
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

pub fn build_pmtiles_from_temp(
    root: &Path,
    output: &Path,
    format: TileFormat,
    min_z: u8,
    max_z: u8,
    mut progress: Option<&mut SingleProgress>,
    stats: &TileStats,
) -> Result<u64> {
    let partial = partial_output(output);
    if partial.exists() {
        fs::remove_file(&partial).with_context(|| format!("remove existing {:?}", partial))?;
    }
    let mut writer = Writer::open(&partial, format, min_z, max_z)?;
    let mut n_written = 0;
    let total_temp_tiles = count_pmtiles_temp_tiles(root, min_z, max_z)?;
    if let Some(progress) = progress.as_deref_mut() {
        progress.start_build(total_temp_tiles, stats);
    }
    for z in min_z..=max_z {
        let files = collect_zoom_temp_files(root, z)?;
        if files.is_empty() {
            continue;
        }
        for (_, tile, path) in files {
            let data = fs::read(&path).with_context(|| format!("read {:?}", path))?;
            writer
                .add_tile(tile.z, tile.x, tile.y, &data)
                .context("add PMTiles tile")?;
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
