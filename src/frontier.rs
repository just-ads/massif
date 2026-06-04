use pmtiles::{TileCoord, TileId};

use anyhow::{Context, Result};

use crate::tile::{lat_to_tile_y_xyz, lon_to_tile_x};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct TileJob {
    pub z: u8,
    pub x: u32,
    pub y: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct TileBounds {
    pub z: u8,
    pub x0: u32,
    pub x1: u32,
    pub y0: u32,
    pub y1: u32,
}

impl TileBounds {
    pub fn contains(&self, tile: TileJob) -> bool {
        tile.z == self.z
            && tile.x >= self.x0
            && tile.x <= self.x1
            && tile.y >= self.y0
            && tile.y <= self.y1
    }
}

pub fn tile_key(tile: TileJob) -> u64 {
    ((tile.z as u64) << 58) | ((tile.x as u64) << 29) | tile.y as u64
}

pub fn pmtiles_tile_id(tile: TileJob) -> Result<u64> {
    Ok(TileId::from(TileCoord::new(tile.z, tile.x, tile.y).context("TileCoord")?).value())
}

pub fn initial_frontier(
    west_lon: f64,
    south_lat: f64,
    east_lon: f64,
    north_lat: f64,
    z: u8,
) -> Vec<TileJob> {
    let x0 = lon_to_tile_x(west_lon, z);
    let x1 = lon_to_tile_x(east_lon, z);
    let y0 = lat_to_tile_y_xyz(north_lat, z);
    let y1 = lat_to_tile_y_xyz(south_lat, z);
    let mut frontier = Vec::new();
    for x in x0..=x1 {
        for y in y0..=y1 {
            frontier.push(TileJob { z, x, y });
        }
    }
    frontier
}

pub fn bounds_for_zoom(
    west_lon: f64,
    south_lat: f64,
    east_lon: f64,
    north_lat: f64,
    z: u8,
) -> TileBounds {
    TileBounds {
        z,
        x0: lon_to_tile_x(west_lon, z),
        x1: lon_to_tile_x(east_lon, z),
        y0: lat_to_tile_y_xyz(north_lat, z),
        y1: lat_to_tile_y_xyz(south_lat, z),
    }
}

pub fn bounds_by_zoom(
    west_lon: f64,
    south_lat: f64,
    east_lon: f64,
    north_lat: f64,
    min_z: u8,
    max_z: u8,
) -> Vec<TileBounds> {
    (min_z..=max_z)
        .map(|z| bounds_for_zoom(west_lon, south_lat, east_lon, north_lat, z))
        .collect()
}

pub fn append_children_in_bounds(
    tile: TileJob,
    next_frontier: &mut Vec<TileJob>,
    max_z: u8,
    bounds: &[TileBounds],
) {
    if tile.z >= max_z {
        return;
    }
    let z = tile.z + 1;
    let Some(next_bounds) = bounds.iter().find(|b| b.z == z) else {
        return;
    };
    let x = tile.x * 2;
    let y = tile.y * 2;
    for child in [
        TileJob { z, x, y },
        TileJob { z, x: x + 1, y },
        TileJob { z, x, y: y + 1 },
        TileJob { z, x: x + 1, y: y + 1 },
    ] {
        if next_bounds.contains(child) {
            next_frontier.push(child);
        }
    }
}

pub fn bounded_descendant_count(tile: TileJob, bounds: &[TileBounds], max_z: u8) -> u64 {
    if tile.z >= max_z {
        return 0;
    }

    let mut total = 0u64;
    for z in (tile.z + 1)..=max_z {
        let Some(bounds) = bounds.iter().find(|b| b.z == z) else {
            continue;
        };
        let scale = 1u64 << (z - tile.z);
        let x0 = tile.x as u64 * scale;
        let x1 = (tile.x as u64 + 1) * scale - 1;
        let y0 = tile.y as u64 * scale;
        let y1 = (tile.y as u64 + 1) * scale - 1;

        let ix0 = x0.max(bounds.x0 as u64);
        let ix1 = x1.min(bounds.x1 as u64);
        let iy0 = y0.max(bounds.y0 as u64);
        let iy1 = y1.min(bounds.y1 as u64);
        if ix0 <= ix1 && iy0 <= iy1 {
            total += (ix1 - ix0 + 1) * (iy1 - iy0 + 1);
        }
    }
    total
}

pub fn parse_tile_filename(name: &str) -> Option<TileJob> {
    let mut parts = name.split('_');
    let z = parts.next()?.parse().ok()?;
    let x = parts.next()?.parse().ok()?;
    let y = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(TileJob { z, x, y })
}

pub fn tile_filename(tile: TileJob) -> String {
    format!("{}_{}_{}", tile.z, tile.x, tile.y)
}
