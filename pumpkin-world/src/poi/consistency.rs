//! 「方块 → POI」的自动索引扫描，对应原版
//! `PoiManager.checkConsistencyWithBlocks` 里的 section 遍历
//! (`/root/Vanilla/src/net/minecraft/world/entity/ai/village/poi/PoiManager.java:193-215`)。
//!
//! 原版在两条路径上维护 POI 索引：
//! - 区块从磁盘读出来时，逐 section 调 `checkConsistencyWithBlocks`
//!   (`/root/Vanilla/src/net/minecraft/world/level/chunk/storage/SerializableChunkData.java:190`)，
//!   按方块实况重建整段索引；
//! - 单个方块状态变化时，走 `Level.setBlock` 末尾的
//!   `updatePOIOnBlockStateChange` (`Level.java:258`)，由
//!   `ServerLevel.updatePOIOnBlockStateChange` (`ServerLevel.java:1290-1307`)
//!   做增量的 remove + add。
//!
//! 这里只提供「扫描」这一半纯函数逻辑；接线在 `pumpkin` crate 的世界侧。

use pumpkin_data::{Block, BlockStateId};
use pumpkin_util::math::position::BlockPos;

use super::types;
use super::{PoiEntry, PoiType};
use crate::chunk::ChunkSections;
use crate::chunk::palette::BlockPalette;

/// 原版 `PoiTypes.forState` 的状态 id 版本
/// (`/root/Vanilla/src/net/minecraft/world/entity/ai/village/poi/PoiTypes.java:82-84`)。
///
/// 原版直接用 `BlockState` 当 map 的 key，Pumpkin 这里从状态 id 反查方块再交给
/// [`types::for_state`]，因为只有床需要区分同一方块的不同状态。
#[must_use]
pub fn poi_type_for_state(state_id: BlockStateId) -> Option<&'static PoiType> {
    types::for_state(Block::from_state_id(state_id), state_id)
}

/// 原版 `PoiManager.mayHavePoi` (`PoiManager.java:206-208`)：先问调色板，
/// 绝大多数 section 一个 POI 都没有，可以整段跳过 4096 格遍历。
#[must_use]
fn may_have_poi(section: &BlockPalette) -> bool {
    section.maybe_has(|state_id| poi_type_for_state(state_id).is_some())
}

/// 扫出该区块当前成立的全部 POI，对应原版
/// `PoiManager.updateFromSection` (`PoiManager.java:210-215`) 在整个区块的
/// 所有 section 上跑一遍。
///
/// 返回的记录票据都是满的（`PoiEntry::new` 走
/// `PoiRecord.maxTickets`，`PoiRecord.java:37-39`）。已经在索引里的同类型记录
/// 会被 `PoiStorage::rebuild_chunk` 原样保留、不会被这些满票据覆盖，对应原版
/// `PoiSection.refresh` 复用旧 `PoiRecord` 的那一步（`PoiSection.java:163-167`）。
#[must_use]
pub fn scan_chunk(sections: &ChunkSections, chunk_x: i32, chunk_z: i32) -> Vec<PoiEntry> {
    let base_x = chunk_x * BlockPalette::SIZE as i32;
    let base_z = chunk_z * BlockPalette::SIZE as i32;
    let mut found = Vec::new();

    // 用具名绑定持有读锁，避免把 `RwLockReadGuard` 直接放进 `for` 的被迭代
    // 表达式里（`significant_drop_in_scrutinee`）。
    let block_sections = sections.block_sections.read().unwrap();
    for (section_index, section) in block_sections.iter().enumerate() {
        // 原版 `checkConsistencyWithBlocks` 的两个分支都先过 `mayHavePoi`。
        if !may_have_poi(section) {
            continue;
        }
        let base_y = sections.min_y + (section_index * BlockPalette::SIZE) as i32;

        for relative_y in 0..BlockPalette::SIZE {
            for relative_z in 0..BlockPalette::SIZE {
                for relative_x in 0..BlockPalette::SIZE {
                    let state_id = section.get(relative_x, relative_y, relative_z);
                    if let Some(poi_type) = poi_type_for_state(state_id) {
                        let pos = BlockPos::new(
                            base_x + relative_x as i32,
                            base_y + relative_y as i32,
                            base_z + relative_z as i32,
                        );
                        found.push(PoiEntry::new(pos, poi_type.name));
                    }
                }
            }
        }
    }

    found
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECTION_COUNT: usize = 24;
    const MIN_Y: i32 = -64;

    fn empty_sections() -> ChunkSections {
        ChunkSections::new(SECTION_COUNT, MIN_Y)
    }

    #[test]
    fn an_empty_chunk_yields_no_poi() {
        assert!(scan_chunk(&empty_sections(), 0, 0).is_empty());
    }

    #[test]
    fn workstations_are_indexed_with_absolute_positions() {
        let sections = empty_sections();
        // 区块 (2, -3) 内的一个高炉，y = 70。
        let blast_furnace = BlockPos::new(2 * 16 + 5, 70, -3 * 16 + 11);
        sections.set_block_absolute_y(5, 70, 11, Block::BLAST_FURNACE.default_state.id);

        let found = scan_chunk(&sections, 2, -3);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].pos(), blast_furnace);
        assert_eq!(found[0].poi_type, "minecraft:armorer");
        // 新扫到的记录票据是满的（armorer 的 maxTickets = 1）。
        assert_eq!(found[0].free_tickets, 1);
    }

    #[test]
    fn poi_below_zero_is_indexed() {
        let sections = empty_sections();
        sections.set_block_absolute_y(0, -40, 0, Block::BELL.default_state.id);

        let found = scan_chunk(&sections, 0, 0);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].pos(), BlockPos::new(0, -40, 0));
        assert_eq!(found[0].poi_type, "minecraft:meeting");
        // meeting 的 maxTickets = 32 (`PoiTypes.java:105`)。
        assert_eq!(found[0].free_tickets, 32);
    }

    #[test]
    fn only_the_bed_head_is_indexed() {
        let sections = empty_sections();
        let mut heads = 0;
        for state in Block::WHITE_BED.states {
            sections.set_block_absolute_y(0, 64, 0, state.id);
            let found = scan_chunk(&sections, 0, 0);
            if let Some(entry) = found.first() {
                assert_eq!(entry.poi_type, "minecraft:home");
                heads += 1;
            }
        }
        // 床有 head 也有 foot，所以两种结果都该出现过。
        assert!(heads > 0 && heads < Block::WHITE_BED.states.len());
    }

    #[test]
    fn non_poi_blocks_are_skipped() {
        let sections = empty_sections();
        sections.set_block_absolute_y(1, 64, 1, Block::STONE.default_state.id);
        assert!(scan_chunk(&sections, 0, 0).is_empty());
    }

    #[test]
    fn every_poi_block_in_one_chunk_is_found() {
        let sections = empty_sections();
        let blocks = [
            &Block::BLAST_FURNACE,
            &Block::SMOKER,
            &Block::CARTOGRAPHY_TABLE,
            &Block::BREWING_STAND,
            &Block::COMPOSTER,
            &Block::BARREL,
            &Block::FLETCHING_TABLE,
            &Block::CAULDRON,
            &Block::LECTERN,
            &Block::STONECUTTER,
            &Block::LOOM,
            &Block::SMITHING_TABLE,
            &Block::GRINDSTONE,
            &Block::BELL,
            &Block::BEEHIVE,
            &Block::BEE_NEST,
            &Block::LODESTONE,
            &Block::LIGHTNING_ROD,
        ];
        for (i, block) in blocks.iter().enumerate() {
            sections.set_block_absolute_y(i, 64, 0, block.default_state.id);
        }

        let found = scan_chunk(&sections, 0, 0);
        assert_eq!(found.len(), blocks.len());
    }
}
