//! POI 索引的一致性维护，对应原版的两条维护路径。
//!
//! - 方块变更：`Level.setBlock` 末尾调 `updatePOIOnBlockStateChange`
//!   (`/root/Vanilla/src/net/minecraft/world/level/Level.java:258`)，实现在
//!   `ServerLevel.updatePOIOnBlockStateChange`
//!   (`/root/Vanilla/src/net/minecraft/server/level/ServerLevel.java:1290-1307`)：
//!   旧类型与新类型相同就直接返回，否则先 `remove` 再 `add`。
//! - 区块加载：区块从磁盘读出时逐 section 调
//!   `PoiManager.checkConsistencyWithBlocks`
//!   (`/root/Vanilla/src/net/minecraft/world/level/chunk/storage/SerializableChunkData.java:190`)，
//!   按方块实况重建该段索引
//!   (`/root/Vanilla/src/net/minecraft/world/entity/ai/village/poi/PoiManager.java:193-204`)。
//!
//! 方块状态到 POI 类型的映射表在 `pumpkin_world::poi::types::for_state`，
//! 对应原版 `PoiTypes.forState`（`PoiTypes.java:82-84`）。

use pumpkin_data::BlockStateId;
use pumpkin_util::math::position::BlockPos;
use pumpkin_world::poi::{poi_type_for_state, scan_chunk};
use tracing::trace;

use super::World;

impl World {
    /// 原版 `ServerLevel.updatePOIOnBlockStateChange`
    /// (`ServerLevel.java:1290-1307`)。
    ///
    /// 只在 POI 类型真的变了的时候才动索引：原版先比较 `forState(oldState)` 与
    /// `forState(newState)`，相等（含两边都不是 POI）就直接返回
    /// (`ServerLevel.java:1292-1295`)。这一步很重要 —— 方块只是换了个状态
    /// （比如高炉点火、木桶开合）时索引不该被推倒重建，否则会把村民已经占用的
    /// 票据一并清掉。
    ///
    /// 锁的注意事项：`portal_poi` 是 `tokio::sync::Mutex`，不可重入。调用点必须
    /// 已经跑完并释放了 `on_state_replaced` 之类会自己上这把锁的回调，所以这个
    /// 钩子放在 `set_block_state` 的最末尾，与原版
    /// `Level.setBlock` 的调用位置一致（`Level.java:258`）。
    pub async fn update_poi_on_block_state_change(
        &self,
        position: &BlockPos,
        old_state_id: BlockStateId,
        new_state_id: BlockStateId,
    ) {
        let old_type = poi_type_for_state(old_state_id);
        let new_type = poi_type_for_state(new_state_id);

        // `ServerLevel.java:1292-1295`：类型没变就什么都不做。
        if old_type.map(|poi_type| poi_type.name) == new_type.map(|poi_type| poi_type.name) {
            return;
        }

        let mut poi_storage = self.portal_poi.lock().await;
        // `ServerLevel.java:1297-1300`：旧类型存在就摘掉旧记录。
        if old_type.is_some() {
            poi_storage.remove(position);
        }
        // `ServerLevel.java:1301-1306`：新类型存在就登记新记录。
        if let Some(poi_type) = new_type {
            poi_storage.add(*position, poi_type.name);
            trace!(
                "Registered POI {} at {:?}",
                poi_type.name, position
            );
        }
    }

    /// 消化本 tick 内新加载/新生成的区块，对它们重建 POI 索引。
    ///
    /// 这是原版 `SerializableChunkData.read` 里那串
    /// `checkConsistencyWithBlocks` 调用的等价物
    /// (`SerializableChunkData.java:190`)。原版在读盘的同一个调用里逐 section
    /// 处理；Pumpkin 的区块加载是异步的，统一通过 `ChunkListener` 广播「区块已
    /// 就绪」，所以这里在世界 tick 里收口，一次处理整个区块的所有 section。
    ///
    /// 原版靠 `PoiSection.isValid` 跳过已经重建过的段（`PoiSection.java:144-152`）。
    /// Pumpkin 的区域存储不保留这个标记，所以每次区块就绪都会重扫一遍；
    /// `PoiStorage::rebuild_chunk` 会保留仍然成立的同类型记录及其票据，所以
    /// 重扫是幂等的，不会把村民占用的工作站或床释放掉。
    pub async fn tick_poi_chunk_loads(&self) {
        // 先把通道抽干再上锁：`try_recv` 是同步的，不该跟 POI 锁交错持有。
        let mut scanned = Vec::new();
        while let Ok((position, chunk_weak)) = self.poi_chunk_listener.try_recv() {
            let Some(chunk) = chunk_weak.upgrade() else {
                continue;
            };
            let found = scan_chunk(&chunk.section, chunk.x, chunk.z);
            scanned.push((position, found));
        }

        if scanned.is_empty() {
            return;
        }

        let mut poi_storage = self.portal_poi.lock().await;
        for (position, found) in scanned {
            if poi_storage.rebuild_chunk(position.x, position.y, &found) {
                trace!(
                    "Rebuilt POI index for chunk {:?}: {} record(s)",
                    position,
                    found.len()
                );
            }
        }
    }
}
