use std::collections::HashMap;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use pumpkin_util::math::position::BlockPos;
use pumpkin_util::math::vector3::Vector3;
use serde::{Deserialize, Serialize};

pub mod types;

pub use types::PoiType;
pub use types::max_tickets_of;

/// POI type identifier for nether portals
pub const POI_TYPE_NETHER_PORTAL: &str = "minecraft:nether_portal";

/// Vanilla `PoiManager.Occupancy` (`PoiManager.java:259-273`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Occupancy {
    /// `HAS_SPACE(PoiRecord::hasSpace)` (`PoiManager.java:260`).
    HasSpace,
    /// `IS_OCCUPIED(PoiRecord::isOccupied)` (`PoiManager.java:261`).
    IsOccupied,
    /// `ANY(poiRecord -> true)` (`PoiManager.java:262`).
    Any,
}

impl Occupancy {
    /// Vanilla `Occupancy.getTest` (`PoiManager.java:270-272`).
    #[must_use]
    pub fn test(self, entry: &PoiEntry) -> bool {
        match self {
            Self::HasSpace => entry.has_space(),
            Self::IsOccupied => entry.is_occupied(),
            Self::Any => true,
        }
    }
}

/// MCA format constants
const SECTOR_SIZE: usize = 4096;
const REGION_SIZE: usize = 32;
const CHUNK_COUNT: usize = REGION_SIZE * REGION_SIZE;
const HEADER_SIZE: usize = SECTOR_SIZE * 2; // Location table + timestamp table

/// Compression type for MCA format
const COMPRESSION_ZLIB: u8 = 2;

// Data version for 1.21
const DATA_VERSION: i32 = 3955;

/// A single Point of Interest entry (serializable)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoiEntry {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    #[serde(rename = "type")]
    pub poi_type: String,
    /// Vanilla `PoiRecord.Packed.CODEC` defaults a missing `free_tickets` field
    /// to zero when decoding (`PoiRecord.java:99-103`).
    #[serde(default)]
    pub free_tickets: i32,
}

impl PoiEntry {
    #[must_use]
    pub fn new_portal(pos: BlockPos) -> Self {
        Self::new(pos, POI_TYPE_NETHER_PORTAL)
    }

    /// Vanilla `PoiRecord(BlockPos, Holder<PoiType>, Runnable)`
    /// (`/root/Vanilla/src/net/minecraft/world/entity/ai/village/poi/PoiRecord.java:37-39`):
    /// a fresh record starts with `freeTickets = poiType.maxTickets()`.
    #[must_use]
    pub fn new(pos: BlockPos, poi_type: &str) -> Self {
        Self {
            x: pos.0.x,
            y: pos.0.y,
            z: pos.0.z,
            free_tickets: max_tickets_of(poi_type),
            poi_type: poi_type.to_string(),
        }
    }

    #[must_use]
    pub const fn pos(&self) -> BlockPos {
        BlockPos(Vector3::new(self.x, self.y, self.z))
    }

    /// `PoiType.maxTickets` for this record's type
    /// (`PoiTypes.java:91-111` via `PoiType.java:11`).
    #[must_use]
    pub fn max_tickets(&self) -> i32 {
        max_tickets_of(&self.poi_type)
    }

    /// Vanilla `PoiRecord.acquireTicket` (`PoiRecord.java:51-58`).
    ///
    /// Fails when no tickets are free; this is what stops two villagers from
    /// claiming the same bed, since `minecraft:home` has `maxTickets = 1`
    /// (`PoiTypes.java:104`).
    pub const fn acquire_ticket(&mut self) -> bool {
        if self.free_tickets <= 0 {
            return false;
        }
        self.free_tickets -= 1;
        true
    }

    /// Vanilla `PoiRecord.releaseTicket` (`PoiRecord.java:60-67`).
    pub fn release_ticket(&mut self) -> bool {
        if self.free_tickets >= self.max_tickets() {
            return false;
        }
        self.free_tickets += 1;
        true
    }

    /// Vanilla `PoiRecord.hasSpace` (`PoiRecord.java:69-71`).
    #[must_use]
    pub const fn has_space(&self) -> bool {
        self.free_tickets > 0
    }

    /// Vanilla `PoiRecord.isOccupied` (`PoiRecord.java:73-75`).
    #[must_use]
    pub fn is_occupied(&self) -> bool {
        self.free_tickets != self.max_tickets()
    }
}

/// POI section data (serializable) - vanilla format
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PoiSectionData {
    #[serde(default)]
    pub valid: i8,
    #[serde(default)]
    pub records: Vec<PoiEntry>,
}

/// POI chunk data (serializable) - vanilla format
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PoiChunkData {
    pub data_version: i32,
    /// Sections keyed by Y section coordinate (e.g., "-1", "0", "1", "4")
    pub sections: HashMap<String, PoiSectionData>,
}

/// POI data for a single region (32x32 chunks) using MCA format
#[derive(Debug, Default)]
pub struct PoiRegion {
    /// Entries indexed by position
    entries: HashMap<(i32, i32, i32), PoiEntry>,
    /// Track which chunks are dirty
    dirty_chunks: std::collections::HashSet<(i32, i32)>,
    dirty: bool,
}

impl PoiRegion {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    const fn pos_key(pos: &BlockPos) -> (i32, i32, i32) {
        (pos.0.x, pos.0.y, pos.0.z)
    }

    /// Get chunk index in MCA file (0-1023)
    const fn chunk_index(chunk_x: i32, chunk_z: i32) -> usize {
        let local_x = chunk_x & 31;
        let local_z = chunk_z & 31;
        ((local_z << 5) | local_x) as usize
    }

    /// Returns section key as just the Y section coordinate (like vanilla)
    fn section_key(pos: &BlockPos) -> String {
        let section_y = pos.0.y >> 4;
        section_y.to_string()
    }

    /// Vanilla `PoiSection.add(PoiRecord)`
    /// (`/root/Vanilla/src/net/minecraft/world/entity/ai/village/poi/PoiSection.java:85-99`).
    ///
    /// Returns `false` without touching anything when a record of the *same* type
    /// is already registered here (`PoiSection.java:90-93`). That guard is what
    /// keeps a re-scan of an already-known bed from silently resetting its
    /// `freeTickets` and handing the same bed to a second villager. A record of a
    /// *different* type replaces the old one (vanilla logs a data mismatch and
    /// overwrites, `PoiSection.java:94-96`).
    pub fn add(&mut self, entry: PoiEntry) -> bool {
        let key = (entry.x, entry.y, entry.z);
        if let Some(existing) = self.entries.get(&key)
            && existing.poi_type == entry.poi_type
        {
            return false;
        }
        let chunk_x = entry.x >> 4;
        let chunk_z = entry.z >> 4;
        self.dirty_chunks.insert((chunk_x, chunk_z));
        self.entries.insert(key, entry);
        self.dirty = true;
        true
    }

    /// The record at `pos`, if one is registered (vanilla `PoiSection.getPoiRecord`,
    /// `PoiSection.java:136-138`).
    #[must_use]
    pub fn get(&self, pos: &BlockPos) -> Option<&PoiEntry> {
        self.entries.get(&Self::pos_key(pos))
    }

    /// Vanilla `PoiSection.release` (`PoiSection.java:118-126`) — hands a ticket
    /// back. `None` when nothing is registered here; vanilla throws in that case
    /// (`PoiManager.release`, `PoiManager.java:146-148`).
    pub fn release(&mut self, pos: &BlockPos) -> Option<bool> {
        let entry = self.entries.get_mut(&Self::pos_key(pos))?;
        let released = entry.release_ticket();
        // `PoiSection.release` marks its section dirty even when the record was
        // already full and `PoiRecord.releaseTicket` returned false.
        self.dirty_chunks.insert((pos.0.x >> 4, pos.0.z >> 4));
        self.dirty = true;
        Some(released)
    }

    /// Takes a ticket at `pos` — the mutation half of vanilla `PoiManager.take`
    /// (`PoiManager.java:134-139`, which calls `PoiRecord.acquireTicket`).
    pub fn acquire(&mut self, pos: &BlockPos) -> bool {
        let Some(entry) = self.entries.get_mut(&Self::pos_key(pos)) else {
            return false;
        };
        let acquired = entry.acquire_ticket();
        if acquired {
            self.dirty_chunks.insert((pos.0.x >> 4, pos.0.z >> 4));
            self.dirty = true;
        }
        acquired
    }

    pub fn remove(&mut self, pos: &BlockPos) -> bool {
        let key = Self::pos_key(pos);
        if self.entries.remove(&key).is_some() {
            let chunk_x = pos.0.x >> 4;
            let chunk_z = pos.0.z >> 4;
            self.dirty_chunks.insert((chunk_x, chunk_z));
            self.dirty = true;
            return true;
        }
        false
    }

    #[must_use]
    pub fn get_all(&self) -> Vec<&PoiEntry> {
        self.entries.values().collect()
    }

    #[must_use]
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn mark_clean(&mut self) {
        self.dirty = false;
        self.dirty_chunks.clear();
    }

    /// Group entries by chunk, then create chunk NBT data
    fn get_chunk_data(&self, chunk_x: i32, chunk_z: i32) -> Option<PoiChunkData> {
        let mut sections: HashMap<String, PoiSectionData> = HashMap::new();

        for entry in self.entries.values() {
            let entry_chunk_x = entry.x >> 4;
            let entry_chunk_z = entry.z >> 4;

            if entry_chunk_x != chunk_x || entry_chunk_z != chunk_z {
                continue;
            }

            let section_key = Self::section_key(&entry.pos());
            let section = sections
                .entry(section_key)
                .or_insert_with(|| PoiSectionData {
                    valid: 1,
                    records: Vec::new(),
                });
            section.records.push(entry.clone());
        }

        if sections.is_empty() {
            None
        } else {
            Some(PoiChunkData {
                data_version: DATA_VERSION,
                sections,
            })
        }
    }

    /// Compress chunk data to bytes
    fn compress_chunk_data(chunk_data: &PoiChunkData) -> std::io::Result<Vec<u8>> {
        let mut uncompressed = Vec::new();
        pumpkin_nbt::to_bytes(chunk_data, &mut uncompressed)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;

        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&uncompressed)?;
        encoder.finish()
    }

    /// Decompress chunk data from bytes
    fn decompress_chunk_data(compressed: &[u8]) -> std::io::Result<PoiChunkData> {
        let mut decoder = ZlibDecoder::new(compressed);
        let mut uncompressed = Vec::new();
        decoder.read_to_end(&mut uncompressed)?;

        let is_named = uncompressed.len() >= 3
            && uncompressed[0] == 0x0a
            && uncompressed[1] == 0x00
            && uncompressed[2] == 0x00;
        if is_named {
            pumpkin_nbt::from_bytes(Cursor::new(uncompressed))
        } else {
            pumpkin_nbt::from_bytes_unnamed(Cursor::new(uncompressed))
        }
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
    }

    pub fn save(&mut self, path: &Path) -> std::io::Result<()> {
        if !self.dirty {
            return Ok(());
        }

        if self.entries.is_empty() {
            // Don't save empty regions, delete the file if it exists
            if path.exists() {
                std::fs::remove_file(path)?;
            }
            self.dirty = false;
            self.dirty_chunks.clear();
            return Ok(());
        }

        // Build all chunk data
        let mut chunk_data_map: HashMap<usize, Vec<u8>> = HashMap::new();

        // Collect all unique chunks that have entries
        let mut chunks_with_data: std::collections::HashSet<(i32, i32)> =
            std::collections::HashSet::new();
        for entry in self.entries.values() {
            chunks_with_data.insert((entry.x >> 4, entry.z >> 4));
        }

        for (chunk_x, chunk_z) in &chunks_with_data {
            if let Some(chunk_data) = self.get_chunk_data(*chunk_x, *chunk_z) {
                let compressed = Self::compress_chunk_data(&chunk_data)?;
                let index = Self::chunk_index(*chunk_x, *chunk_z);
                chunk_data_map.insert(index, compressed);
            }
        }

        // Build MCA file
        let mut location_table = [0u32; CHUNK_COUNT];
        let mut timestamp_table = [0u32; CHUNK_COUNT];
        let mut sector_data: Vec<Vec<u8>> = Vec::new();

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as u32);

        // Start after header (2 sectors)
        let mut current_sector: u32 = 2;

        for index in 0..CHUNK_COUNT {
            if let Some(compressed) = chunk_data_map.get(&index) {
                // Calculate sector count needed
                let data_len = compressed.len() + 5; // 4 bytes length + 1 byte compression + data
                let sector_count = data_len.div_ceil(SECTOR_SIZE) as u32;

                // Build padded sector data
                let mut padded = Vec::with_capacity(sector_count as usize * SECTOR_SIZE);
                let length = (compressed.len() + 1) as u32; // +1 for compression byte
                padded.extend_from_slice(&length.to_be_bytes());
                padded.push(COMPRESSION_ZLIB);
                padded.extend_from_slice(compressed);
                // Pad to sector boundary
                padded.resize(sector_count as usize * SECTOR_SIZE, 0);

                location_table[index] = (current_sector << 8) | sector_count;
                timestamp_table[index] = timestamp;
                sector_data.push(padded);

                current_sector += sector_count;
            }
        }

        // Write file
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut file = std::fs::File::create(path)?;

        // Write location table
        for loc in &location_table {
            file.write_all(&loc.to_be_bytes())?;
        }

        // Write timestamp table
        for ts in &timestamp_table {
            file.write_all(&ts.to_be_bytes())?;
        }

        // Write chunk data
        for data in &sector_data {
            file.write_all(data)?;
        }

        self.dirty = false;
        self.dirty_chunks.clear();
        Ok(())
    }

    pub fn load(path: &Path) -> std::io::Result<Self> {
        if !path.exists() {
            return Ok(Self::new());
        }

        let file_data = std::fs::read(path)?;
        if file_data.len() < HEADER_SIZE {
            return Ok(Self::new());
        }

        let mut region = Self::new();

        // Parse location table
        for index in 0..CHUNK_COUNT {
            let offset = index * 4;
            let location = u32::from_be_bytes([
                file_data[offset],
                file_data[offset + 1],
                file_data[offset + 2],
                file_data[offset + 3],
            ]);

            let sector_offset = (location >> 8) as usize;
            let sector_count = (location & 0xFF) as usize;

            if sector_offset == 0 || sector_count == 0 {
                continue;
            }

            let byte_offset = sector_offset * SECTOR_SIZE;
            let byte_end = byte_offset + sector_count * SECTOR_SIZE;

            if byte_end > file_data.len() {
                continue;
            }

            // Read chunk data
            let chunk_bytes = &file_data[byte_offset..byte_end];
            if chunk_bytes.len() < 5 {
                continue;
            }

            let length = u32::from_be_bytes([
                chunk_bytes[0],
                chunk_bytes[1],
                chunk_bytes[2],
                chunk_bytes[3],
            ]) as usize;
            let compression = chunk_bytes[4];

            if compression != COMPRESSION_ZLIB || length < 1 || length > chunk_bytes.len() - 4 {
                continue;
            }

            let compressed = &chunk_bytes[5..5 + length - 1];

            match Self::decompress_chunk_data(compressed) {
                Ok(chunk_data) => {
                    for (_section_key, section) in chunk_data.sections {
                        for entry in section.records {
                            let key = (entry.x, entry.y, entry.z);
                            region.entries.insert(key, entry);
                        }
                    }
                }
                Err(e) => {
                    warn!("Failed to parse POI chunk at index {index}: {e}");
                }
            }
        }

        region.dirty = false;
        Ok(region)
    }
}

/// Region-based POI storage using MCA format
pub struct PoiStorage {
    /// Path to the poi folder
    folder: PathBuf,
    /// Loaded regions, keyed by (`region_x`, `region_z`)
    regions: HashMap<(i32, i32), PoiRegion>,
}

impl PoiStorage {
    #[must_use]
    pub fn new(poi_folder: PathBuf) -> Self {
        Self {
            folder: poi_folder,
            regions: HashMap::new(),
        }
    }

    const fn region_coords(pos: &BlockPos) -> (i32, i32) {
        let chunk_x = pos.0.x >> 4;
        let chunk_z = pos.0.z >> 4;
        (chunk_x >> 5, chunk_z >> 5)
    }

    fn region_path(&self, rx: i32, rz: i32) -> PathBuf {
        self.folder.join(format!("r.{rx}.{rz}.mca"))
    }

    fn get_or_load_region(&mut self, rx: i32, rz: i32) -> &mut PoiRegion {
        let path = self.region_path(rx, rz);
        self.regions.entry((rx, rz)).or_insert_with(|| {
            PoiRegion::load(&path).unwrap_or_else(|e| {
                if path.exists() {
                    warn!("Failed to load POI region {}: {}", path.display(), e);
                }
                PoiRegion::new()
            })
        })
    }

    /// Adds a POI record, returning whether it was newly registered.
    ///
    /// A same-type record already at `pos` is left unchanged, including its
    /// tickets, just as vanilla `PoiSection.add` does (`PoiSection.java:85-99`).
    pub fn add(&mut self, pos: BlockPos, poi_type: &str) -> bool {
        let (rx, rz) = Self::region_coords(&pos);
        self.get_or_load_region(rx, rz)
            .add(PoiEntry::new(pos, poi_type))
    }

    /// Adds a nether-portal POI.
    pub fn add_portal(&mut self, pos: BlockPos) -> bool {
        self.add(pos, POI_TYPE_NETHER_PORTAL)
    }

    /// Removes the POI record at `pos`, if any.
    pub fn remove(&mut self, pos: &BlockPos) -> bool {
        let (rx, rz) = Self::region_coords(pos);
        self.get_or_load_region(rx, rz).remove(pos)
    }

    /// Returns the record at `pos`, loading its region if needed.
    #[must_use]
    pub fn get(&mut self, pos: &BlockPos) -> Option<&PoiEntry> {
        let (rx, rz) = Self::region_coords(pos);
        self.get_or_load_region(rx, rz).get(pos)
    }

    /// Returns the registered POI type at `pos`, loading its region if needed.
    #[must_use]
    pub fn get_type(&mut self, pos: &BlockPos) -> Option<&str> {
        self.get(pos).map(|entry| entry.poi_type.as_str())
    }

    /// Tests the POI at `pos` against a type predicate.
    ///
    /// This is the storage counterpart of vanilla `PoiManager.exists`
    /// (`PoiManager.java:150-152`). A persisted type Pumpkin does not know is
    /// not a registered `PoiType`, so it cannot satisfy the predicate.
    pub fn exists(
        &mut self,
        pos: &BlockPos,
        mut type_predicate: impl FnMut(&PoiType) -> bool,
    ) -> bool {
        self.get(pos)
            .and_then(|entry| types::by_name(&entry.poi_type))
            .is_some_and(|poi_type| type_predicate(poi_type))
    }

    /// Tests whether `pos` contains the given registered POI type.
    ///
    /// This mirrors vanilla `PoiManager.existsAtPosition`
    /// (`PoiManager.java:84-86`).
    pub fn exists_at_position(&mut self, poi_type: &str, pos: &BlockPos) -> bool {
        self.exists(pos, |registered_type| registered_type.name == poi_type)
    }

    /// Attempts to acquire a ticket for the POI at `pos`.
    ///
    /// Returns `false` for absent records and records without free tickets.
    /// Vanilla's matching record mutation is `PoiRecord.acquireTicket`
    /// (`PoiRecord.java:51-58`).
    pub fn acquire(&mut self, pos: &BlockPos) -> bool {
        let (rx, rz) = Self::region_coords(pos);
        self.get_or_load_region(rx, rz).acquire(pos)
    }

    /// Attempts to release a ticket for the POI at `pos`.
    ///
    /// Returns `None` when no record is registered. Vanilla throws in that case
    /// (`PoiManager.release`, `PoiManager.java:146-148`); the optional result
    /// keeps this storage API safe for future world callers.
    pub fn release(&mut self, pos: &BlockPos) -> Option<bool> {
        let (rx, rz) = Self::region_coords(pos);
        self.get_or_load_region(rx, rz).release(pos)
    }

    /// Returns POI records in the inclusive X/Z square around `center` for a
    /// caller-supplied type predicate and vanilla occupancy predicate.
    ///
    /// This mirrors the final filter in vanilla `PoiManager.getInSquare`
    /// (`PoiManager.java:88-94`): Y is intentionally unrestricted. Results use
    /// a deterministic type/position order because this region-file storage uses
    /// hash maps, whereas vanilla does not expose a stable order within a POI
    /// section.
    #[must_use]
    pub fn get_entries_in_square_by(
        &mut self,
        center: BlockPos,
        radius: i32,
        mut type_predicate: impl FnMut(&PoiType) -> bool,
        occupancy: Occupancy,
    ) -> Vec<PoiEntry> {
        self.query_entries_in_square(
            center,
            radius,
            |entry| types::by_name(&entry.poi_type).is_some_and(&mut type_predicate),
            occupancy,
        )
    }

    /// Returns POI records in the inclusive three-dimensional Euclidean range
    /// around `center` for a caller-supplied type predicate and vanilla
    /// occupancy predicate.
    ///
    /// This follows vanilla `PoiManager.getInRange` (`PoiManager.java:96-99`),
    /// including its `distanceSquared <= radiusSquared` boundary.
    #[must_use]
    pub fn get_entries_in_range_by(
        &mut self,
        center: BlockPos,
        radius: i32,
        type_predicate: impl FnMut(&PoiType) -> bool,
        occupancy: Occupancy,
    ) -> Vec<PoiEntry> {
        if radius < 0 {
            return Vec::new();
        }
        let radius_squared = i64::from(radius) * i64::from(radius);
        let mut results = self.get_entries_in_square_by(center, radius, type_predicate, occupancy);
        results.retain(|entry| Self::squared_distance(entry.pos(), center) <= radius_squared);
        results
    }

    /// Returns POI records in the inclusive X/Z square around `center` for an
    /// exact stored type name and vanilla occupancy predicate.
    ///
    /// Unlike `get_entries_in_square_by`, this also returns persisted unknown
    /// types when `poi_type` is `None`, matching the raw stored-record behavior.
    #[must_use]
    pub fn get_entries_in_square(
        &mut self,
        center: BlockPos,
        radius: i32,
        poi_type: Option<&str>,
        occupancy: Occupancy,
    ) -> Vec<PoiEntry> {
        self.query_entries_in_square(
            center,
            radius,
            |entry| poi_type.is_none_or(|type_name| entry.poi_type == type_name),
            occupancy,
        )
    }

    /// Returns POI records in the inclusive three-dimensional Euclidean range
    /// around `center` for an exact stored type name and vanilla occupancy
    /// predicate.
    #[must_use]
    pub fn get_entries_in_range(
        &mut self,
        center: BlockPos,
        radius: i32,
        poi_type: Option<&str>,
        occupancy: Occupancy,
    ) -> Vec<PoiEntry> {
        if radius < 0 {
            return Vec::new();
        }
        let radius_squared = i64::from(radius) * i64::from(radius);
        let mut results = self.get_entries_in_square(center, radius, poi_type, occupancy);
        results.retain(|entry| Self::squared_distance(entry.pos(), center) <= radius_squared);
        results
    }

    /// Returns POI positions in the inclusive X/Z square around `center`.
    ///
    /// This portal-compatible wrapper queries every occupancy state. General
    /// callers that need ticket filtering should use `get_entries_in_square`.
    #[must_use]
    pub fn get_in_square(
        &mut self,
        center: BlockPos,
        radius: i32,
        poi_type: Option<&str>,
    ) -> Vec<BlockPos> {
        self.get_entries_in_square(center, radius, poi_type, Occupancy::Any)
            .into_iter()
            .map(|entry| entry.pos())
            .collect()
    }

    /// Returns POI positions in the inclusive X/Z square around `center` using
    /// the supplied vanilla occupancy predicate.
    #[must_use]
    pub fn get_in_square_with_occupancy(
        &mut self,
        center: BlockPos,
        radius: i32,
        poi_type: Option<&str>,
        occupancy: Occupancy,
    ) -> Vec<BlockPos> {
        self.get_entries_in_square(center, radius, poi_type, occupancy)
            .into_iter()
            .map(|entry| entry.pos())
            .collect()
    }

    /// Returns POI positions in the inclusive three-dimensional Euclidean range
    /// around `center`.
    ///
    /// This portal-compatible wrapper queries every occupancy state. General
    /// callers that need ticket filtering should use `get_entries_in_range`.
    #[must_use]
    pub fn get_in_range(
        &mut self,
        center: BlockPos,
        radius: i32,
        poi_type: Option<&str>,
    ) -> Vec<BlockPos> {
        self.get_entries_in_range(center, radius, poi_type, Occupancy::Any)
            .into_iter()
            .map(|entry| entry.pos())
            .collect()
    }

    /// Returns POI positions in the inclusive three-dimensional Euclidean range
    /// around `center` using the supplied vanilla occupancy predicate.
    #[must_use]
    pub fn get_in_range_with_occupancy(
        &mut self,
        center: BlockPos,
        radius: i32,
        poi_type: Option<&str>,
        occupancy: Occupancy,
    ) -> Vec<BlockPos> {
        self.get_entries_in_range(center, radius, poi_type, occupancy)
            .into_iter()
            .map(|entry| entry.pos())
            .collect()
    }

    /// Finds and claims one matching POI in a single mutable-storage operation.
    ///
    /// The type and position predicates are evaluated before the ticket is
    /// acquired. Holding `&mut self` across selection and mutation means callers
    /// sharing this storage behind one lock cannot observe the same free ticket.
    /// Candidate selection uses this storage's deterministic type/position order;
    /// vanilla exposes only the first record produced by its section streams.
    /// This is the atomic storage equivalent of vanilla `PoiManager.take`
    /// (`PoiManager.java:134-139`).
    #[must_use]
    pub fn take_by(
        &mut self,
        center: BlockPos,
        radius: i32,
        mut type_predicate: impl FnMut(&PoiType) -> bool,
        mut filter: impl FnMut(&PoiType, BlockPos) -> bool,
    ) -> Option<BlockPos> {
        let Some((min_x, max_x, min_z, max_z)) = Self::square_bounds(center, radius) else {
            return None;
        };
        let radius_squared = i64::from(radius) * i64::from(radius);
        let (min_rx, min_rz) = Self::region_coords(&BlockPos::new(min_x, center.0.y, min_z));
        let (max_rx, max_rz) = Self::region_coords(&BlockPos::new(max_x, center.0.y, max_z));

        let mut candidates = Vec::new();
        for rx in min_rx..=max_rx {
            for rz in min_rz..=max_rz {
                let region = self.get_or_load_region(rx, rz);
                candidates.extend(region.get_all().into_iter().filter_map(|entry| {
                    let poi_type = types::by_name(&entry.poi_type)?;
                    (Self::matches_square(entry, center, radius)
                        && Self::squared_distance(entry.pos(), center) <= radius_squared
                        && type_predicate(poi_type)
                        && Occupancy::HasSpace.test(entry))
                    .then_some((entry.pos(), poi_type))
                }));
            }
        }
        candidates
            .sort_unstable_by_key(|(pos, poi_type)| (poi_type.name, pos.0.x, pos.0.y, pos.0.z));

        for (pos, poi_type) in candidates {
            if filter(poi_type, pos) && self.acquire(&pos) {
                return Some(pos);
            }
        }
        None
    }

    /// Finds and claims one matching POI using an exact stored type name.
    ///
    /// Use [`Self::take_by`] when the caller needs a vanilla-style type
    /// predicate. This wrapper keeps filtering and ticket acquisition inside the
    /// same storage operation.
    #[must_use]
    pub fn take(
        &mut self,
        center: BlockPos,
        radius: i32,
        poi_type: Option<&str>,
        mut filter: impl FnMut(&str, BlockPos) -> bool,
    ) -> Option<BlockPos> {
        self.take_by(
            center,
            radius,
            |registered_type| poi_type.is_none_or(|type_name| registered_type.name == type_name),
            |registered_type, pos| filter(registered_type.name, pos),
        )
    }

    fn query_entries_in_square(
        &mut self,
        center: BlockPos,
        radius: i32,
        mut type_predicate: impl FnMut(&PoiEntry) -> bool,
        occupancy: Occupancy,
    ) -> Vec<PoiEntry> {
        let Some((min_x, max_x, min_z, max_z)) = Self::square_bounds(center, radius) else {
            return Vec::new();
        };
        let (min_rx, min_rz) = Self::region_coords(&BlockPos::new(min_x, center.0.y, min_z));
        let (max_rx, max_rz) = Self::region_coords(&BlockPos::new(max_x, center.0.y, max_z));

        let mut results = Vec::new();
        for rx in min_rx..=max_rx {
            for rz in min_rz..=max_rz {
                let region = self.get_or_load_region(rx, rz);
                results.extend(
                    region
                        .get_all()
                        .into_iter()
                        .filter(|entry| {
                            Self::matches_square(entry, center, radius)
                                && type_predicate(entry)
                                && occupancy.test(entry)
                        })
                        .cloned(),
                );
            }
        }
        Self::sort_entries(&mut results);
        results
    }

    const fn square_bounds(center: BlockPos, radius: i32) -> Option<(i32, i32, i32, i32)> {
        if radius < 0 {
            return None;
        }
        let Some(min_x) = center.0.x.checked_sub(radius) else {
            return None;
        };
        let Some(max_x) = center.0.x.checked_add(radius) else {
            return None;
        };
        let Some(min_z) = center.0.z.checked_sub(radius) else {
            return None;
        };
        let Some(max_z) = center.0.z.checked_add(radius) else {
            return None;
        };
        Some((min_x, max_x, min_z, max_z))
    }

    fn matches_square(entry: &PoiEntry, center: BlockPos, radius: i32) -> bool {
        (i64::from(entry.x) - i64::from(center.0.x)).abs() <= i64::from(radius)
            && (i64::from(entry.z) - i64::from(center.0.z)).abs() <= i64::from(radius)
    }

    fn squared_distance(pos: BlockPos, center: BlockPos) -> i64 {
        let x = i64::from(pos.0.x) - i64::from(center.0.x);
        let y = i64::from(pos.0.y) - i64::from(center.0.y);
        let z = i64::from(pos.0.z) - i64::from(center.0.z);
        x * x + y * y + z * z
    }

    fn sort_entries(entries: &mut [PoiEntry]) {
        entries.sort_unstable_by(Self::compare_entries);
    }

    fn compare_entries(left: &PoiEntry, right: &PoiEntry) -> std::cmp::Ordering {
        left.poi_type
            .cmp(&right.poi_type)
            .then_with(|| left.x.cmp(&right.x))
            .then_with(|| left.y.cmp(&right.y))
            .then_with(|| left.z.cmp(&right.z))
    }

    pub fn save_all(&mut self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.folder)?;

        let mut saved = 0;
        for ((rx, rz), region) in &mut self.regions {
            if region.is_dirty() {
                let path = self.folder.join(format!("r.{rx}.{rz}.mca"));
                region.save(&path)?;
                saved += 1;
            }
        }

        if saved > 0 {
            info!("Saved {saved} POI region(s)");
        }
        Ok(())
    }

    /// Get count of loaded regions
    #[must_use]
    pub fn loaded_region_count(&self) -> usize {
        self.regions.len()
    }

    /// Get total POI count across all loaded regions
    #[must_use]
    pub fn total_poi_count(&self) -> usize {
        self.regions.values().map(|r| r.get_all().len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(x: i32, y: i32, z: i32) -> BlockPos {
        BlockPos::new(x, y, z)
    }

    fn test_storage() -> (tempfile::TempDir, PoiStorage) {
        let temp_dir = tempfile::tempdir().unwrap();
        let storage = PoiStorage::new(temp_dir.path().join("poi"));
        (temp_dir, storage)
    }

    #[test]
    fn poi_entry() {
        let entry = PoiEntry::new_portal(pos(100, 64, 200));
        assert_eq!(entry.x, 100);
        assert_eq!(entry.y, 64);
        assert_eq!(entry.z, 200);
        assert_eq!(entry.poi_type, POI_TYPE_NETHER_PORTAL);
        assert_eq!(entry.free_tickets, 0);
    }

    #[test]
    fn known_poi_types_start_with_their_vanilla_ticket_capacity() {
        let home = PoiEntry::new(pos(100, 64, 200), "minecraft:home");
        assert_eq!(home.free_tickets, 1);
        assert!(home.has_space());
        assert!(!home.is_occupied());

        let meeting = PoiEntry::new(pos(100, 64, 201), "minecraft:meeting");
        assert_eq!(meeting.free_tickets, 32);
        assert!(meeting.has_space());
        assert!(!meeting.is_occupied());
    }

    #[test]
    fn take_and_release_manage_home_ticket() {
        let home = pos(0, 64, 0);
        let (_temp_dir, mut storage) = test_storage();
        assert!(storage.add(home, "minecraft:home"));

        assert_eq!(
            storage.take(home, 0, Some("minecraft:home"), |_, _| true),
            Some(home)
        );
        assert_eq!(storage.get(&home).map(|entry| entry.free_tickets), Some(0));
        assert_eq!(
            storage.take(home, 0, Some("minecraft:home"), |_, _| true),
            None
        );
        assert_eq!(storage.release(&home), Some(true));
        assert_eq!(storage.get(&home).map(|entry| entry.free_tickets), Some(1));
        assert_eq!(storage.release(&home), Some(false));
        assert_eq!(storage.release(&pos(8, 64, 8)), None);
    }

    #[test]
    fn get_and_get_type_load_records_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let home = pos(0, 64, 0);

        let mut writer = PoiStorage::new(dir.path().join("poi"));
        assert!(writer.add(home, "minecraft:home"));
        writer.save_all().unwrap();

        let mut reader = PoiStorage::new(dir.path().join("poi"));
        assert_eq!(reader.get_type(&home), Some("minecraft:home"));
        assert_eq!(reader.get(&home).map(|entry| entry.free_tickets), Some(1));
    }

    #[test]
    fn duplicate_add_preserves_claimed_ticket() {
        let home = pos(0, 64, 0);
        let (_temp_dir, mut storage) = test_storage();
        assert!(storage.add(home, "minecraft:home"));
        assert!(storage.acquire(&home));
        assert!(!storage.add(home, "minecraft:home"));
        assert_eq!(storage.get(&home).map(|entry| entry.free_tickets), Some(0));
    }

    #[test]
    fn different_type_add_replaces_the_record() {
        let poi_pos = pos(0, 64, 0);
        let (_temp_dir, mut storage) = test_storage();
        assert!(storage.add(poi_pos, "minecraft:home"));
        assert!(storage.acquire(&poi_pos));

        assert!(storage.add(poi_pos, "minecraft:meeting"));
        assert_eq!(storage.get_type(&poi_pos), Some("minecraft:meeting"));
        assert_eq!(
            storage.get(&poi_pos).map(|entry| entry.free_tickets),
            Some(32)
        );
    }

    #[test]
    fn missing_ticket_field_defaults_to_zero_when_deserialized() {
        let entry: PoiEntry =
            serde_json::from_str(r#"{"x":0,"y":64,"z":0,"type":"minecraft:home"}"#).unwrap();
        assert_eq!(entry.free_tickets, 0);
        assert!(entry.is_occupied());
    }

    #[test]
    fn occupancy_filters_match_ticket_semantics() {
        let home = pos(0, 64, 0);
        let portal = pos(1, 64, 0);
        let (_temp_dir, mut storage) = test_storage();
        assert!(storage.add(home, "minecraft:home"));
        assert!(storage.add_portal(portal));

        assert_eq!(
            storage.get_in_square_with_occupancy(home, 2, None, Occupancy::Any),
            vec![home, portal]
        );
        assert_eq!(
            storage.get_in_square_with_occupancy(home, 2, None, Occupancy::HasSpace),
            vec![home]
        );
        assert!(
            storage
                .get_in_square_with_occupancy(home, 2, None, Occupancy::IsOccupied)
                .is_empty()
        );

        assert!(storage.acquire(&home));
        assert!(
            storage
                .get_in_square_with_occupancy(home, 2, None, Occupancy::HasSpace)
                .is_empty()
        );
        assert_eq!(
            storage.get_in_square_with_occupancy(home, 2, None, Occupancy::IsOccupied),
            vec![home]
        );
    }

    #[test]
    #[expect(clippy::similar_names)]
    fn square_and_range_queries_use_vanilla_boundaries() {
        let center = pos(0, 64, 0);
        let square_edge = pos(2, -128, 2);
        let circle_edge = pos(3, 64, 4);
        let outside_range_y = pos(0, 70, 0);
        let outside_square = pos(3, 64, 0);
        let (_temp_dir, mut storage) = test_storage();
        for entry in [square_edge, circle_edge, outside_range_y, outside_square] {
            assert!(storage.add(entry, "minecraft:home"));
        }

        let square =
            storage.get_in_square_with_occupancy(center, 2, Some("minecraft:home"), Occupancy::Any);
        assert_eq!(square, vec![outside_range_y, square_edge]);
        assert_eq!(
            storage.get_in_range(center, 5, Some("minecraft:home")),
            vec![circle_edge]
        );
    }

    #[test]
    fn type_predicate_queries_and_exists_use_registered_metadata() {
        let center = pos(0, 64, 0);
        let home = pos(0, 64, 0);
        let farmer = pos(1, 64, 0);
        let meeting = pos(2, 64, 0);
        let (_temp_dir, mut storage) = test_storage();
        assert!(storage.add(home, "minecraft:home"));
        assert!(storage.add(farmer, "minecraft:farmer"));
        assert!(storage.add(meeting, "minecraft:meeting"));

        assert!(storage.exists_at_position("minecraft:home", &home));
        assert!(storage.exists(&farmer, |poi_type| poi_type.acquirable_job_site));
        assert!(!storage.exists(&home, |poi_type| poi_type.acquirable_job_site));

        assert_eq!(
            storage
                .get_entries_in_square_by(center, 2, |poi_type| poi_type.village, Occupancy::Any)
                .into_iter()
                .map(|entry| entry.pos())
                .collect::<Vec<_>>(),
            vec![farmer, home, meeting]
        );
        assert_eq!(
            storage
                .get_entries_in_range_by(
                    center,
                    2,
                    |poi_type| poi_type.acquirable_job_site,
                    Occupancy::Any,
                )
                .into_iter()
                .map(|entry| entry.pos())
                .collect::<Vec<_>>(),
            vec![farmer]
        );
    }

    #[test]
    fn take_by_applies_type_and_position_predicates_before_claiming() {
        let center = pos(0, 64, 0);
        let farmer = pos(1, 64, 0);
        let armorer = pos(2, 64, 0);
        let unavailable = pos(3, 64, 0);
        let (_temp_dir, mut storage) = test_storage();
        assert!(storage.add(farmer, "minecraft:farmer"));
        assert!(storage.add(armorer, "minecraft:armorer"));
        assert!(storage.add(unavailable, "minecraft:butcher"));
        assert!(storage.acquire(&unavailable));

        assert_eq!(
            storage.take_by(
                center,
                3,
                |poi_type| poi_type.acquirable_job_site,
                |poi_type, poi_pos| poi_type.name == "minecraft:farmer" && poi_pos == farmer,
            ),
            Some(farmer)
        );
        assert_eq!(
            storage.get(&farmer).map(|entry| entry.free_tickets),
            Some(0)
        );
        assert_eq!(
            storage.get(&armorer).map(|entry| entry.free_tickets),
            Some(1)
        );
        assert_eq!(
            storage.get(&unavailable).map(|entry| entry.free_tickets),
            Some(0)
        );
    }

    #[test]
    fn take_by_selects_across_region_boundaries() {
        let center = pos(511, 64, 0);
        let first = pos(510, 64, 0);
        let second = pos(513, 64, 0);
        let (_temp_dir, mut storage) = test_storage();
        assert!(storage.add(first, "minecraft:farmer"));
        assert!(storage.add(second, "minecraft:farmer"));

        assert_eq!(
            storage.take_by(
                center,
                3,
                |poi_type| poi_type.acquirable_job_site,
                |_, _| true,
            ),
            Some(first)
        );
        assert_eq!(
            storage.take_by(
                center,
                3,
                |poi_type| poi_type.acquirable_job_site,
                |_, _| true,
            ),
            Some(second)
        );
    }

    #[test]
    fn poi_region() {
        let mut region = PoiRegion::new();
        region.add(PoiEntry::new_portal(pos(100, 64, 200)));
        region.add(PoiEntry::new_portal(pos(101, 64, 200)));

        assert_eq!(region.get_all().len(), 2);
        assert!(region.is_dirty());

        region.remove(&pos(100, 64, 200));
        assert_eq!(region.get_all().len(), 1);
    }

    #[test]
    fn poi_storage_mca() {
        let dir = tempfile::tempdir().unwrap();
        let poi_dir = dir.path().join("poi");
        let mut storage = PoiStorage::new(poi_dir.clone());

        storage.add_portal(pos(100, 64, 100));
        storage.add_portal(pos(110, 64, 100));
        storage.add_portal(pos(1000, 64, 1000)); // Different region

        let results = storage.get_in_square(pos(105, 64, 100), 16, Some(POI_TYPE_NETHER_PORTAL));
        assert_eq!(results.len(), 2);

        storage.save_all().unwrap();

        // Verify .mca file was created
        let mca_path = poi_dir.join("r.0.0.mca");
        assert!(mca_path.exists());

        // Reload and verify
        let mut storage2 = PoiStorage::new(poi_dir);
        let results2 = storage2.get_in_square(pos(105, 64, 100), 16, Some(POI_TYPE_NETHER_PORTAL));
        assert_eq!(results2.len(), 2);
    }
}
