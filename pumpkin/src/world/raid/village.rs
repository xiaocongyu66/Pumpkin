//! 突袭用的村庄判定 —— 走真正的 POI 查询。
//!
//! # 与原版的对应
//!
//! 原版 `ServerLevel.isVillage`（`ServerLevel.java:1313-1315`）就是
//! `isCloseToVillage(pos, 1)`；`isCloseToVillage`（`ServerLevel.java:1321-1327`）
//! 先拒掉大于 6 的 section 距离，再问 `PoiManager.sectionsToVillage`
//! （`PoiManager.java:163-166`）。那张距离场的源点判定是 `PoiManager.isVillageCenter`
//! （`PoiManager.java:168-174`）：section 里存在带 `#minecraft:village` tag 且
//! `IS_OCCUPIED` 的 POI 记录。
//!
//! `Raids.createOrExtendRaid`（`Raids.java:118`）走同一条路取村庄中心：
//! `getInRange(e -> e.is(PoiTypeTags.VILLAGE), raidPosition, 64, IS_OCCUPIED)`。
//!
//! 本模块把这两处都换成对世界 POI 存储的真实查询：
//!
//! - `#minecraft:village` tag → [`PoiType::village`] 标签位。`poi/types.rs` 已按
//!   `village.json` 标好（13 种工作站 + `home` + `meeting`），所以这里按标签筛，
//!   不硬编码类型名列表。
//! - `IS_OCCUPIED` → [`Occupancy::IsOccupied`]，即 `PoiRecord.isOccupied`。
//! - `getInRange` / `getInSquare` → `PoiStorage` 的同名查询，边界判定见
//!   [`QueryShape`]。
//!
//! # 距离场：Chebyshev 距离就是原版答案，不是近似
//!
//! 原版 `DistanceTracker` 继承 `SectionTracker`（`SectionTracker.java:16-60`）：
//! 向 3×3×3 邻域的全部 26 个邻居传播，每跳代价固定 1，源点 section 取 0，7 表示
//! 「够远了」（`PoiManager.java:228-257`）。均匀代价 + 全邻域的 BFS 不动点，正是
//! section 空间里到最近源点 section 的 Chebyshev 距离。所以
//! [`section_distance_to_nearest`] 算出来的数和原版逐格相同。
//!
//! # 仍与原版有出入的地方
//!
//! 1. **没有预算好的距离场。** 原版在 tick 里增量维护 `DistanceTracker`，查询
//!    O(1)；这里每次判定都现查一遍 POI。语义一致，代价不一致 —— 为此每个入口只
//!    取刚好够用的半径（见 [`village_poi_snapshot`]），需要连查多个位置的调用方
//!    应当取一次快照后走纯函数版本。
//! 2. **可见范围反而更宽。** 原版距离场只覆盖已加载的 POI section；`PoiStorage`
//!    直接读 region 文件，因此也能看到未加载区域里的 POI。
//! 3. **集会点（钟）尚未计入。** 原版村民认领钟会写 `MEETING_POINT` 记忆槽并取票
//!    （`VillagerGoalPackages.java:99`）；Pumpkin 村民还没有对应字段，所以
//!    `minecraft:meeting` 记录目前不会变成 `IS_OCCUPIED`。床位和 13 种工作站都已
//!    接上票据认领（见 `entity/passive/villager/poi.rs`），村庄判定由它们支撑。

use std::sync::Arc;

use pumpkin_util::math::position::BlockPos;
use pumpkin_util::math::vector3::Vector3;
use pumpkin_world::poi::{Occupancy, PoiEntry, PoiType};

use crate::world::World;

/// 原版 `ServerLevel.isCloseToVillage` 对超过 6 个 section 的请求直接拒绝
/// （`ServerLevel.java:1321-1327`）；这也是 `DistanceTracker` 的 `levelCount - 1`
/// （`PoiManager.java:230` 的 `super(7, 16, 256)`）。
pub const MAX_VILLAGE_SECTION_DISTANCE: i32 = 6;

/// 原版 `Raids.createOrExtendRaid` 在突袭位置 64 格内收集被占用的村庄 POI
/// （`Raids.java:118`）。
pub const RAID_POI_SEARCH_RADIUS: i32 = 64;

/// 原版 `Raid.VILLAGE_RADIUS_BUFFER`（`Raid.java:94`）。
pub const VILLAGE_RADIUS_BUFFER: i32 = 16;

/// 方块坐标到 section 坐标（原版 `SectionPos.blockToSectionCoord`）。
const fn to_section(coord: i32) -> i32 {
    coord >> 4
}

/// `#minecraft:village` tag 的成员判定。
///
/// 对应原版 `e -> e.is(PoiTypeTags.VILLAGE)`（`Raids.java:118`、
/// `PoiManager.java:173`）。tag 成员表在 `poi/types.rs` 里按
/// `resources/data/minecraft/tags/point_of_interest_type/village.json` 标注。
fn is_village_poi_type(poi_type: &PoiType) -> bool {
    poi_type.village
}

/// 查询的边界形状 —— 原版这两种都在用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryShape {
    /// `PoiManager.getInRange`（`PoiManager.java:96-99`）：三维欧氏球，
    /// `distSqr <= radius²`。`Raids.java:118` 用的是这个。
    Range,
    /// `PoiManager.getInSquare`（`PoiManager.java:88-94`）：X/Z 方形，Y 不限。
    Square,
}

/// `center` 附近被占用的村庄 POI 位置快照。
///
/// 这是 `PoiManager.getInRange(#minecraft:village, .., IS_OCCUPIED)`
/// （`Raids.java:118`）的直译：按 [`PoiType::village`] 标签筛类型，用
/// [`Occupancy::IsOccupied`] 筛占用状态。
///
/// 取一次快照再交给下面的纯函数，是为了让「同一批 POI 回答多个位置」的调用方
/// （例如 `Raid.moveRaidCenterToNearbyVillageSection` 要扫 125 个 section 中心）
/// 只付一次查询代价。
///
/// `World::portal_poi` 就是这个世界的 POI 存储本体（`world/mod.rs:184` 用
/// `level_folder.poi_folder` 建的，和原版 `poi/` region 目录同一份），字段名只是
/// 沿用了它当初唯一的调用方。村庄和下界传送门共用一份存储，正如原版共用一个
/// `PoiManager`。
pub async fn village_poi_snapshot(
    world: &Arc<World>,
    center: BlockPos,
    radius: i32,
    shape: QueryShape,
) -> Vec<BlockPos> {
    let mut storage = world.portal_poi.lock().await;
    let entries = match shape {
        QueryShape::Range => storage.get_entries_in_range_by(
            center,
            radius,
            is_village_poi_type,
            Occupancy::IsOccupied,
        ),
        QueryShape::Square => storage.get_entries_in_square_by(
            center,
            radius,
            is_village_poi_type,
            Occupancy::IsOccupied,
        ),
    };
    drop(storage);
    entries.iter().map(PoiEntry::pos).collect()
}

/// [`QueryShape`] 的边界判定，和 `PoiStorage` 内部用的判定同构。
///
/// 仅供测试断言查询形状语义；生产路径直接走 `PoiStorage` 自身的边界判定。
#[cfg(test)]
fn in_query_bounds(pos: BlockPos, center: BlockPos, radius: i32, shape: QueryShape) -> bool {
    let dx = i64::from(pos.0.x) - i64::from(center.0.x);
    let dy = i64::from(pos.0.y) - i64::from(center.0.y);
    let dz = i64::from(pos.0.z) - i64::from(center.0.z);
    let radius = i64::from(radius);
    // `getInSquare` 的 X/Z 方形约束，Y 不限（`PoiManager.java:91-93`）。
    if dx.abs() > radius || dz.abs() > radius {
        return false;
    }
    match shape {
        QueryShape::Square => true,
        // `getInRange` 追加的 `distSqr <= radius²`（`PoiManager.java:97-98`）。
        QueryShape::Range => dx * dx + dy * dy + dz * dz <= radius * radius,
    }
}

/// 覆盖到 `MAX_VILLAGE_SECTION_DISTANCE` 个 section 所需的方块半径。
///
/// 距离场按 section 记数，`section_distance <= 6` 最远可以由 7 个 section 之外的
/// 方块坐标满足（同一 section 内的偏移最多 15 格），所以用方形查询多取一个
/// section 的余量，再由 Chebyshev 计算收敛到精确答案。
const fn village_search_radius() -> i32 {
    (MAX_VILLAGE_SECTION_DISTANCE + 2) * 16
}

/// 原版 `ServerLevel.sectionsToVillage`（`ServerLevel.java:1329-1331`）。
///
/// 返回到最近村庄 POI 的 section 距离；没有任何村庄 POI 时返回 `None`
/// （对应原版距离场给出 7，即「超出 6 的上限」）。
pub async fn sections_to_village(world: &Arc<World>, pos: &BlockPos) -> Option<i32> {
    let positions =
        village_poi_snapshot(world, *pos, village_search_radius(), QueryShape::Square).await;
    section_distance_to_nearest(pos, &positions)
}

/// 纯粹的 section 距离归约，拆出来便于脱离 `World` 测试，也便于复用同一份快照。
///
/// 结果等于原版 `DistanceTracker` 的定点解，理由见模块文档。
#[must_use]
pub fn section_distance_to_nearest(pos: &BlockPos, poi_positions: &[BlockPos]) -> Option<i32> {
    poi_positions
        .iter()
        .map(|poi| {
            let dx = (to_section(poi.0.x) - to_section(pos.0.x)).abs();
            let dy = (to_section(poi.0.y) - to_section(pos.0.y)).abs();
            let dz = (to_section(poi.0.z) - to_section(pos.0.z)).abs();
            dx.max(dy).max(dz)
        })
        .min()
}

/// 原版 `ServerLevel.isCloseToVillage`（`ServerLevel.java:1321-1327`）。
///
/// `section_distance > 6` 时直接返回 `false`，和原版的提前返回一致。
pub async fn is_close_to_village(
    world: &Arc<World>,
    pos: &BlockPos,
    section_distance: i32,
) -> bool {
    if section_distance > MAX_VILLAGE_SECTION_DISTANCE {
        return false;
    }
    sections_to_village(world, pos)
        .await
        .is_some_and(|distance| distance <= section_distance)
}

/// 原版 `ServerLevel.isVillage`（`ServerLevel.java:1313-1315`）：
/// `isCloseToVillage(pos, 1)`。
pub async fn is_village(world: &Arc<World>, pos: &BlockPos) -> bool {
    is_close_to_village(world, pos, 1).await
}

/// [`is_village`] 的纯函数版本，供已经持有快照的调用方使用。
///
/// 快照的半径必须由 [`snapshot_radius_for_blocks`] 或
/// [`snapshot_radius_for_sections`] 算出来并覆盖到 `pos`，否则答案会偏大。
#[must_use]
pub fn is_village_in_snapshot(pos: &BlockPos, poi_positions: &[BlockPos]) -> bool {
    section_distance_to_nearest(pos, poi_positions).is_some_and(|distance| distance <= 1)
}

/// 覆盖 `center` 周围 `section_radius` 个 section 的全部查询所需的方块半径。
///
/// 用于一次取快照、再对多个 section 中心做 [`is_village_in_snapshot`] 的调用方
/// （`Raid.moveRaidCenterToNearbyVillageSection`，`Raid.java:375-378`）。
#[must_use]
pub const fn snapshot_radius_for_sections(section_radius: i32) -> i32 {
    village_search_radius() + (section_radius + 1) * 16
}

/// 覆盖 `center` 周围 `block_radius` 格内任意位置的村庄判定所需的快照半径。
#[must_use]
pub const fn snapshot_radius_for_blocks(block_radius: i32) -> i32 {
    village_search_radius() + block_radius
}

/// 掠夺者剔除半径：`Raid.RAID_REMOVAL_THRESHOLD_SQR` 是 12544，即 112 格
/// （`Raid.java:108`）。离突袭中心比这更远的掠夺者会先被 `update_raiders` 的距离
/// 判定剔除（`Raid.java:418`），根本走不到 `in_village` 那一步，所以按这个半径取的
/// 快照足够回答全部有意义的村庄判定。
pub const RAID_REMOVAL_THRESHOLD: i32 = 112;

/// 原版 `Raids.createOrExtendRaid` 的中心计算（`Raids.java:118-131`）。
///
/// 取突袭位置 64 格内被占用的村庄 POI 求平均并向下取整；一个都没有时回退到
/// `raid_position`，和原版 `count == 0` 的分支一致。
pub async fn raid_center_for(world: &Arc<World>, raid_position: &BlockPos) -> BlockPos {
    let positions = village_poi_snapshot(
        world,
        *raid_position,
        RAID_POI_SEARCH_RADIUS,
        QueryShape::Range,
    )
    .await;
    average_center(raid_position, &positions)
}

/// [`raid_center_for`] 的纯函数部分，脱离 `World` 可测。
///
/// 原版把各 POI 位置累加进 `Vec3`，乘 `1/count`，再交给 `BlockPos.containing`
/// （逐分量向下取整）—— `Raids.java:120-131`。
#[must_use]
pub fn average_center(fallback: &BlockPos, poi_positions: &[BlockPos]) -> BlockPos {
    let count = poi_positions.len();
    if count == 0 {
        return *fallback;
    }
    let mut total = Vector3::new(0.0f64, 0.0f64, 0.0f64);
    for poi in poi_positions {
        total = total.add_raw(f64::from(poi.0.x), f64::from(poi.0.y), f64::from(poi.0.z));
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "POI 数量远低于 f64 的精确整数范围"
    )]
    let scale = 1.0 / count as f64;
    BlockPos::floored(total.x * scale, total.y * scale, total.z * scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_poi_means_no_village() {
        let pos = BlockPos::new(0, 64, 0);
        assert_eq!(section_distance_to_nearest(&pos, &[]), None);
        assert!(!is_village_in_snapshot(&pos, &[]));
    }

    #[test]
    fn same_section_is_distance_zero() {
        let pos = BlockPos::new(4, 64, 7);
        let poi = BlockPos::new(9, 68, 2);
        assert_eq!(section_distance_to_nearest(&pos, &[poi]), Some(0));
        assert!(is_village_in_snapshot(&pos, &[poi]));
    }

    #[test]
    fn adjacent_section_is_distance_one() {
        // x = 20 落在 section 1，x = 4 落在 section 0。
        let pos = BlockPos::new(4, 64, 4);
        let poi = BlockPos::new(20, 64, 4);
        assert_eq!(section_distance_to_nearest(&pos, &[poi]), Some(1));
        // isVillage == isCloseToVillage(pos, 1)，所以距离 1 仍算村庄。
        assert!(is_village_in_snapshot(&pos, &[poi]));
    }

    #[test]
    fn two_sections_away_is_not_a_village() {
        let pos = BlockPos::new(0, 64, 0);
        let poi = BlockPos::new(40, 64, 0);
        assert_eq!(section_distance_to_nearest(&pos, &[poi]), Some(2));
        assert!(!is_village_in_snapshot(&pos, &[poi]));
    }

    #[test]
    fn distance_is_chebyshev_not_manhattan() {
        // x 走 3 个 section、z 也走 3 个，距离仍是 3 —— 原版 26 邻域传播的结果。
        let pos = BlockPos::new(0, 64, 0);
        let poi = BlockPos::new(48, 64, 48);
        assert_eq!(section_distance_to_nearest(&pos, &[poi]), Some(3));
    }

    #[test]
    fn vertical_sections_count_too() {
        let pos = BlockPos::new(0, 64, 0);
        let poi = BlockPos::new(0, 96, 0);
        assert_eq!(section_distance_to_nearest(&pos, &[poi]), Some(2));
    }

    #[test]
    fn nearest_poi_wins() {
        let pos = BlockPos::new(0, 64, 0);
        let far = BlockPos::new(200, 64, 0);
        let near = BlockPos::new(20, 64, 0);
        assert_eq!(section_distance_to_nearest(&pos, &[far, near]), Some(1));
    }

    #[test]
    fn negative_coordinates_floor_toward_negative_infinity() {
        // -1 >> 4 == -1，所以 -1 处的方块落在 section -1，不是 0。
        let pos = BlockPos::new(-1, 64, -1);
        let poi = BlockPos::new(0, 64, 0);
        assert_eq!(section_distance_to_nearest(&pos, &[poi]), Some(1));
    }

    #[test]
    fn search_radius_covers_the_vanilla_section_cap() {
        // 半径必须够让 section 距离 6 的 POI 一定被方形查询捞到：最坏情况是查询点
        // 贴在自己 section 的一端、POI 贴在它那个 section 的另一端。
        let worst_case = MAX_VILLAGE_SECTION_DISTANCE * 16 + 15;
        assert!(village_search_radius() >= worst_case);
    }

    #[test]
    fn snapshot_radius_grows_with_the_section_span() {
        // 扫 5×5×5 个 section 中心（section_radius = 2）时，快照要覆盖到最远那个
        // 中心再往外 6 个 section。
        let radius = snapshot_radius_for_sections(2);
        let farthest_center = BlockPos::new(2 * 16 + 8, 64, 2 * 16 + 8);
        let poi_at_cap = BlockPos::new(
            farthest_center.0.x + MAX_VILLAGE_SECTION_DISTANCE * 16 + 15,
            64,
            farthest_center.0.z,
        );
        assert!(in_query_bounds(
            poi_at_cap,
            BlockPos::new(0, 64, 0),
            radius,
            QueryShape::Square,
        ));
    }

    #[test]
    fn square_bounds_ignore_y_but_range_bounds_do_not() {
        let center = BlockPos::new(0, 64, 0);
        // Y 差 100 格，X/Z 都在半径内。
        let high = BlockPos::new(0, 164, 0);
        assert!(in_query_bounds(high, center, 16, QueryShape::Square));
        assert!(!in_query_bounds(high, center, 16, QueryShape::Range));
    }

    #[test]
    fn range_bounds_are_inclusive_at_the_radius() {
        let center = BlockPos::new(0, 64, 0);
        let on_edge = BlockPos::new(64, 64, 0);
        let past_edge = BlockPos::new(65, 64, 0);
        assert!(in_query_bounds(
            on_edge,
            center,
            RAID_POI_SEARCH_RADIUS,
            QueryShape::Range,
        ));
        assert!(!in_query_bounds(
            past_edge,
            center,
            RAID_POI_SEARCH_RADIUS,
            QueryShape::Range,
        ));
    }

    #[test]
    fn village_tag_selects_homes_and_job_sites_but_not_portals() {
        use pumpkin_world::poi::types;
        assert!(is_village_poi_type(&types::HOME));
        assert!(is_village_poi_type(&types::MEETING));
        assert!(is_village_poi_type(&types::FARMER));
        assert!(!is_village_poi_type(&types::NETHER_PORTAL));
        assert!(!is_village_poi_type(&types::BEEHIVE));
    }

    #[test]
    fn center_falls_back_to_the_raid_position() {
        let fallback = BlockPos::new(11, 65, -7);
        assert_eq!(average_center(&fallback, &[]), fallback);
    }

    #[test]
    fn center_is_the_floored_mean_of_the_pois() {
        let fallback = BlockPos::new(0, 0, 0);
        let pois = [
            BlockPos::new(0, 64, 0),
            BlockPos::new(10, 64, 10),
            BlockPos::new(20, 70, 20),
        ];
        // 平均值恰好是 (10, 66, 10)。
        assert_eq!(average_center(&fallback, &pois), BlockPos::new(10, 66, 10));
    }

    #[test]
    fn center_floors_rather_than_rounds() {
        let fallback = BlockPos::new(0, 0, 0);
        // x 均值 1/2 = 0.5 -> 向下取整成 0；z 均值 3/2 = 1.5 -> 取 1。
        let pois = [BlockPos::new(0, 64, 1), BlockPos::new(1, 64, 2)];
        assert_eq!(average_center(&fallback, &pois), BlockPos::new(0, 64, 1));
    }

    #[test]
    fn center_floors_negative_means_downward() {
        let fallback = BlockPos::new(0, 0, 0);
        // x 均值 -1/2 = -0.5 -> 取 -1，不是 0。
        let pois = [BlockPos::new(0, 64, 0), BlockPos::new(-1, 64, 0)];
        assert_eq!(average_center(&fallback, &pois), BlockPos::new(-1, 64, 0));
    }

    #[test]
    fn constants_match_vanilla() {
        assert_eq!(MAX_VILLAGE_SECTION_DISTANCE, 6);
        assert_eq!(RAID_POI_SEARCH_RADIUS, 64);
        assert_eq!(VILLAGE_RADIUS_BUFFER, 16);
    }
}
