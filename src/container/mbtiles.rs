use std::path::Path;

use std::collections::HashSet;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::frontier::{tile_key, TileJob};
use crate::tile_format::TileFormat;

/// MBTiles writer.
///
/// MBTiles uses TMS tile ordering: y=0 is at the *south*.
/// XYZ → TMS: tms_y = (2^z − 1) − xyz_y
///
/// Tiles are committed chunk-by-chunk so interrupted runs can resume.
pub struct MbtilesWriter {
    conn: Connection,
}

impl MbtilesWriter {
    pub fn create(path: &Path, format: TileFormat, min_z: u8, max_z: u8) -> Result<Self> {
        if path.exists() {
            std::fs::remove_file(path)
                .with_context(|| format!("remove existing {:?}", path))?;
        }
        Self::open_initialized(path, format, min_z, max_z)
    }

    pub fn open_or_create(path: &Path, format: TileFormat, min_z: u8, max_z: u8) -> Result<Self> {
        Self::open_initialized(path, format, min_z, max_z)
    }

    fn open_initialized(path: &Path, format: TileFormat, min_z: u8, max_z: u8) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("open {:?}", path))?;

        conn.execute_batch("
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            CREATE TABLE IF NOT EXISTS metadata (name TEXT PRIMARY KEY, value TEXT);
            CREATE TABLE IF NOT EXISTS tiles (
                zoom_level  INTEGER NOT NULL,
                tile_column INTEGER NOT NULL,
                tile_row    INTEGER NOT NULL,
                tile_data   BLOB    NOT NULL,
                PRIMARY KEY (zoom_level, tile_column, tile_row)
            );
            CREATE TABLE IF NOT EXISTS massif_progress (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS massif_frontier (
                zoom_level   INTEGER NOT NULL,
                tile_column  INTEGER NOT NULL,
                tile_row_xyz INTEGER NOT NULL,
                PRIMARY KEY (zoom_level, tile_column, tile_row_xyz)
            );
        ").context("create MBTiles schema")?;

        let mime = match format {
            TileFormat::Webp => "image/webp",
            TileFormat::Png  => "image/png",
        };

        {
            let mut stmt = conn.prepare(
                "INSERT OR REPLACE INTO metadata (name, value) VALUES (?1, ?2)"
            ).context("prepare metadata insert")?;
            for (k, v) in [
                ("name",    "massif"),
                ("format",  mime),
                ("type",    "baselayer"),
                ("version", "1.1"),
                ("minzoom", &min_z.to_string()),
                ("maxzoom", &max_z.to_string()),
            ] {
                stmt.execute(params![k, v]).context("insert metadata")?;
            }
        }

        Ok(Self { conn })
    }

    pub fn progress_status(&self) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM massif_progress WHERE key = 'status'")
            .context("prepare progress status")?;
        let mut rows = stmt.query([]).context("query progress status")?;
        if let Some(row) = rows.next().context("read progress status")? {
            Ok(Some(row.get(0).context("get progress status")?))
        } else {
            Ok(None)
        }
    }

    pub fn current_zoom(&self) -> Result<Option<u8>> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM massif_progress WHERE key = 'current_zoom'")
            .context("prepare current zoom")?;
        let mut rows = stmt.query([]).context("query current zoom")?;
        if let Some(row) = rows.next().context("read current zoom")? {
            let value: String = row.get(0).context("get current zoom")?;
            Ok(Some(value.parse().context("parse current zoom")?))
        } else {
            Ok(None)
        }
    }

    pub fn set_progress(&mut self, status: &str, current_zoom: u8) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO massif_progress (key, value) VALUES ('status', ?1)",
            params![status],
        ).context("set MBTiles progress status")?;
        self.conn.execute(
            "INSERT OR REPLACE INTO massif_progress (key, value) VALUES ('current_zoom', ?1)",
            params![current_zoom.to_string()],
        ).context("set MBTiles current zoom")?;
        Ok(())
    }

    pub fn begin_chunk(&mut self) -> Result<()> {
        self.conn.execute_batch("BEGIN IMMEDIATE").context("begin MBTiles chunk")
    }

    pub fn commit_chunk(&mut self) -> Result<()> {
        self.conn.execute_batch("COMMIT").context("commit MBTiles chunk")
    }

    pub fn rollback_chunk(&mut self) -> Result<()> {
        self.conn.execute_batch("ROLLBACK").context("rollback MBTiles chunk")
    }

    pub fn add_tile(&mut self, z: u8, x: u32, y_xyz: u32, data: &[u8]) -> Result<()> {
        // Flip y from XYZ (north=0) to TMS (south=0)
        let tms_y = (1u32 << z).wrapping_sub(1).wrapping_sub(y_xyz);
        self.conn.execute(
            "INSERT OR REPLACE INTO tiles (zoom_level, tile_column, tile_row, tile_data)
             VALUES (?1, ?2, ?3, ?4)",
            params![z, x, tms_y, data],
        ).context("insert tile")?;
        Ok(())
    }

    pub fn existing_tiles(&mut self, tiles: &[TileJob]) -> Result<HashSet<u64>> {
        self.conn.execute_batch("
            CREATE TEMP TABLE IF NOT EXISTS massif_check_tiles (
                z INTEGER NOT NULL,
                x INTEGER NOT NULL,
                y_xyz INTEGER NOT NULL,
                y_tms INTEGER NOT NULL,
                PRIMARY KEY (z, x, y_xyz)
            );
            DELETE FROM massif_check_tiles;
        ").context("prepare MBTiles existing check table")?;

        {
            let tx = self.conn.transaction().context("begin existing check transaction")?;
            {
                let mut stmt = tx.prepare(
                    "INSERT OR IGNORE INTO massif_check_tiles (z, x, y_xyz, y_tms)
                     VALUES (?1, ?2, ?3, ?4)",
                ).context("prepare existing check insert")?;
                for tile in tiles {
                    let tms_y = xyz_to_tms_y(tile.z, tile.y);
                    stmt.execute(params![tile.z, tile.x, tile.y, tms_y])
                        .context("insert existing check tile")?;
                }
            }
            tx.commit().context("commit existing check transaction")?;
        }

        let mut existing = HashSet::new();
        let mut stmt = self.conn.prepare("
            SELECT c.z, c.x, c.y_xyz
            FROM massif_check_tiles c
            JOIN tiles t
              ON t.zoom_level = c.z
             AND t.tile_column = c.x
             AND t.tile_row = c.y_tms
        ").context("prepare existing tile query")?;
        let mut rows = stmt.query([]).context("query existing tiles")?;
        while let Some(row) = rows.next().context("read existing tile row")? {
            let tile = TileJob {
                z: row.get(0).context("existing z")?,
                x: row.get(1).context("existing x")?,
                y: row.get(2).context("existing y")?,
            };
            existing.insert(tile_key(tile));
        }
        Ok(existing)
    }

    pub fn save_frontier(&mut self, z: u8, frontier: &[TileJob]) -> Result<()> {
        let tx = self.conn.transaction().context("begin save frontier transaction")?;
        tx.execute("DELETE FROM massif_frontier WHERE zoom_level = ?1", params![z])
            .context("delete old MBTiles frontier")?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO massif_frontier (zoom_level, tile_column, tile_row_xyz)
                 VALUES (?1, ?2, ?3)",
            ).context("prepare save MBTiles frontier")?;
            for tile in frontier {
                stmt.execute(params![tile.z, tile.x, tile.y])
                    .context("insert MBTiles frontier")?;
            }
        }
        tx.commit().context("commit save frontier transaction")
    }

    pub fn load_frontier(&self, z: u8) -> Result<Vec<TileJob>> {
        let mut stmt = self.conn.prepare(
            "SELECT zoom_level, tile_column, tile_row_xyz
             FROM massif_frontier
             WHERE zoom_level = ?1
             ORDER BY tile_column, tile_row_xyz",
        ).context("prepare load MBTiles frontier")?;
        let rows = stmt.query_map(params![z], |row| {
            Ok(TileJob {
                z: row.get(0)?,
                x: row.get(1)?,
                y: row.get(2)?,
            })
        }).context("query MBTiles frontier")?;

        let mut frontier = Vec::new();
        for row in rows {
            frontier.push(row.context("read MBTiles frontier row")?);
        }
        Ok(frontier)
    }

    pub fn delete_frontier(&mut self, z: u8) -> Result<()> {
        self.conn
            .execute("DELETE FROM massif_frontier WHERE zoom_level = ?1", params![z])
            .context("delete completed MBTiles frontier")?;
        Ok(())
    }

    pub fn finalize(self) -> Result<()> {
        self.conn.execute_batch("
            CREATE UNIQUE INDEX IF NOT EXISTS tiles_idx ON tiles (zoom_level, tile_column, tile_row);
            DROP TABLE IF EXISTS massif_progress;
            DROP TABLE IF EXISTS massif_frontier;
        ").context("finalize MBTiles")?;
        Ok(())
    }
}

fn xyz_to_tms_y(z: u8, y_xyz: u32) -> u32 {
    (1u32 << z).wrapping_sub(1).wrapping_sub(y_xyz)
}
