use crate::entity::EntityBase;
use crate::entity::decoration::armor_stand::ArmorStandEntity;
use crate::entity::r#type::check_spawn_rules;
use crate::entity::vehicle::boat::BoatEntity;
use crate::entity::vehicle::minecart::MinecartEntity;
use crate::world::World;
use pumpkin_data::biome::Spawner;
use pumpkin_data::chunk::Biome;
use pumpkin_data::entity::{EntityType, MobCategory, SpawnLocation};
use pumpkin_data::tag::Block::{
    MINECRAFT_BUTTONS, MINECRAFT_FIRE, MINECRAFT_FOX_IMMUNE_TO, MINECRAFT_POLAR_BEAR_IMMUNE_TO,
    MINECRAFT_PRESSURE_PLATES, MINECRAFT_PREVENT_MOB_SPAWNING_INSIDE,
    MINECRAFT_SNOW_GOLEM_IMMUNE_TO, MINECRAFT_STRAY_IMMUNE_TO, MINECRAFT_WITHER_IMMUNE_TO,
    MINECRAFT_WITHER_SKELETON_IMMUNE_TO,
};
use pumpkin_data::tag::Fluid::{MINECRAFT_LAVA, MINECRAFT_WATER};
use pumpkin_data::tag::Taggable;
use pumpkin_data::tag::WorldgenBiome::MINECRAFT_REDUCE_WATER_AMBIENT_SPAWNS;
use pumpkin_data::{Block, BlockDirection, BlockId, BlockState};
use pumpkin_util::GameMode;
use pumpkin_util::math::boundingbox::{BoundingBox, EntityDimensions};
use pumpkin_util::math::get_section_cord;
use pumpkin_util::math::position::BlockPos;
use pumpkin_util::math::vector2::Vector2;
use pumpkin_util::math::vector3::Vector3;
use pumpkin_util::random::RandomImpl;
use pumpkin_world::generation::proto_chunk::GenerationCache;
use rand::{Rng, RngExt, rng};
use rustc_hash::FxHashSet;
use std::sync::Arc;

use super::{NATURAL_SPAWN_CHUNK_RANGE, SPAWN_DISTANCE_BLOCK_SQ};

/// Java's `BlockState.isRedstoneConductor` is represented by this cached block-state flag.
#[must_use]
pub(super) fn is_redstone_conductor(state: &BlockState) -> bool {
    state.is_solid_block()
}

/// Vanilla `ChunkMap.anyPlayerCloseEnoughForSpawningInternal`.
#[must_use]
pub fn any_player_close_enough_for_spawning(world: &World, chunk_pos: Vector2<i32>) -> bool {
    let chunk_center_x = ((chunk_pos.x << 4) + 8) as f64;
    let chunk_center_z = ((chunk_pos.y << 4) + 8) as f64;
    for player in world.players.load().iter() {
        if player.gamemode.load() == GameMode::Spectator {
            continue;
        }
        let pos = player.position();
        let dx = chunk_center_x - pos.x;
        let dz = chunk_center_z - pos.z;
        if dx * dx + dz * dz < SPAWN_DISTANCE_BLOCK_SQ {
            return true;
        }
    }
    false
}

/// Whether `chunk_pos` is inside vanilla natural-spawn candidate range
/// (`FixedPlayerDistanceChunkTracker` radius 8 around any player chunk).
#[must_use]
pub fn is_natural_spawn_candidate(world: &World, chunk_pos: Vector2<i32>) -> bool {
    let spectators_generate_chunks = world
        .level_info
        .load()
        .game_rules
        .spectators_generate_chunks;
    for player in world.players.load().iter() {
        if player.gamemode.load() == GameMode::Spectator && !spectators_generate_chunks {
            continue;
        }
        let center = player.get_entity().chunk_pos.load();
        if (chunk_pos.x - center.x)
            .abs()
            .max((chunk_pos.y - center.y).abs())
            <= NATURAL_SPAWN_CHUNK_RANGE
        {
            return true;
        }
    }
    false
}

#[must_use]
pub fn get_nearest_player(pos: &Vector3<f64>, player_positions: &[Vector3<f64>]) -> f64 {
    let mut min_dst_sq = f64::MAX;

    for player_pos in player_positions {
        let cur_dst_sq = player_pos.squared_distance_to_vec(pos);
        if cur_dst_sq < min_dst_sq {
            min_dst_sq = cur_dst_sq;
        }
    }
    min_dst_sq
}

#[must_use]
pub fn is_right_distance_to_player_and_spawn_point(
    world: &World,
    pos: &BlockPos,
    distance: f64,
    chunk_pos: &Vector2<i32>,
    world_spawn: Vector3<f64>,
) -> bool {
    // Vanilla: must be > 24 blocks from any player.
    if distance <= 24. * 24. {
        return false;
    }
    // Vanilla: must be > 24 blocks from world spawn (not world origin).
    if pos
        .to_centered_f64()
        .squared_distance_to(world_spawn.x, world_spawn.y, world_spawn.z)
        <= 24. * 24.
    {
        return false;
    }
    let target_chunk = Vector2::new(get_section_cord(pos.0.x), get_section_cord(pos.0.z));
    can_spawn_entities_in_chunk(*chunk_pos, target_chunk, &world.active_chunks.load())
}

/// Vanilla permits a pack to spill from its origin chunk into another entity-ticking chunk.
#[must_use]
fn can_spawn_entities_in_chunk(
    origin_chunk: Vector2<i32>,
    target_chunk: Vector2<i32>,
    active_chunks: &FxHashSet<Vector2<i32>>,
) -> bool {
    origin_chunk == target_chunk || active_chunks.contains(&target_chunk)
}

#[must_use]
pub fn get_random_spawn_mob_at(
    world: &Arc<World>,
    category: &'static MobCategory,
    block_pos: &BlockPos,
) -> Option<&'static Spawner> {
    let mut random = rng();
    get_random_spawn_mob_at_with_random(world, category, block_pos, &mut random)
}

pub(super) fn get_random_spawn_mob_at_with_random<R: Rng + ?Sized>(
    world: &Arc<World>,
    category: &'static MobCategory,
    block_pos: &BlockPos,
    random: &mut R,
) -> Option<&'static Spawner> {
    // TODO Holder<Biome> holder = level.getBiome(pos);
    let biome = world.level.get_rough_biome(block_pos);
    if category == &MobCategory::WATER_AMBIENT
        && biome.has_tag(&MINECRAFT_REDUCE_WATER_AMBIENT_SPAWNS)
        && random.random::<f32>() < 0.98f32
    {
        return None;
    }

    // TODO isInNetherFortressBounds(pos, level, cetagory, structureManager) then NetherFortressStructure.FORTRESS_ENEMIES
    // TODO structureManager.getAllStructuresAt(pos); ChunkGenerator::getMobsAt
    choose_weighted_spawner(spawn_pool_for_category(biome, category), random)
}

fn spawn_pool_for_category(biome: &Biome, category: &'static MobCategory) -> &'static [Spawner] {
    match category.id {
        id if id == MobCategory::MONSTER.id => biome.spawners.monster,
        id if id == MobCategory::CREATURE.id => biome.spawners.creature,
        id if id == MobCategory::AMBIENT.id => biome.spawners.ambient,
        id if id == MobCategory::AXOLOTLS.id => biome.spawners.axolotls,
        id if id == MobCategory::UNDERGROUND_WATER_CREATURE.id => {
            biome.spawners.underground_water_creature
        }
        id if id == MobCategory::WATER_CREATURE.id => biome.spawners.water_creature,
        id if id == MobCategory::WATER_AMBIENT.id => biome.spawners.water_ambient,
        id if id == MobCategory::MISC.id => biome.spawners.misc,
        _ => panic!(),
    }
}

pub(super) fn spawner_is_in_biome_pool(
    biome: &Biome,
    category: &'static MobCategory,
    spawner: &Spawner,
) -> bool {
    spawn_pool_for_category(biome, category)
        .iter()
        .any(|candidate| {
            candidate.r#type == spawner.r#type
                && candidate.min_count == spawner.min_count
                && candidate.max_count == spawner.max_count
        })
}

fn total_spawner_weight(spawners: &[Spawner]) -> Option<u64> {
    let total = spawners.iter().try_fold(0_u64, |total, spawner| {
        total.checked_add(u64::from(spawner.weight))
    })?;
    (total != 0).then_some(total)
}

#[must_use]
fn select_weighted_spawner(spawners: &[Spawner], mut roll: u64) -> Option<&Spawner> {
    if roll >= total_spawner_weight(spawners)? {
        return None;
    }

    for spawner in spawners {
        let weight = u64::from(spawner.weight);
        if roll < weight {
            return Some(spawner);
        }
        roll -= weight;
    }

    None
}

fn choose_weighted_spawner<'a, R: Rng + ?Sized>(
    spawners: &'a [Spawner],
    random: &mut R,
) -> Option<&'a Spawner> {
    let total = total_spawner_weight(spawners)?;
    select_weighted_spawner(spawners, random.random_range(0..total))
}

pub(super) fn choose_weighted_spawner_with_random_impl<'a, R: RandomImpl>(
    spawners: &'a [Spawner],
    random: &mut R,
) -> Option<&'a Spawner> {
    let total = i32::try_from(total_spawner_weight(spawners)?).ok()?;
    select_weighted_spawner(spawners, random.next_bounded_i32(total) as u64)
}

pub fn is_valid_spawn_position_for_type(
    world: &Arc<World>,
    block_pos: &BlockPos,
    category: &'static MobCategory,
    entity_type: &'static EntityType,
    distance: f64,
    is_thundering: bool,
) -> bool {
    // SpawnPlacements.checkSpawnRules → is_spawn_position_ok + check_spawn_rules below
    // (vanilla EntitySpawnReason.NATURAL light / biome rules).
    if category == &MobCategory::MISC {
        return false;
    }
    if !entity_type.can_spawn_far_from_player
        && distance
            > f64::from(entity_type.category.despawn_distance)
                * f64::from(entity_type.category.despawn_distance)
    {
        return false;
    }
    if !entity_type.summonable {
        return false;
    }
    if !is_spawn_position_ok(world, block_pos, entity_type) {
        return false;
    }
    if !check_spawn_rules(entity_type, world, block_pos, is_thundering) {
        return false;
    }
    // TODO: we should use getSpawnBox, but this is only modified for slimes and magma slimes
    if !world.is_space_empty(BoundingBox::new_from_pos(
        f64::from(block_pos.0.x) + 0.5,
        f64::from(block_pos.0.y),
        f64::from(block_pos.0.z) + 0.5,
        &EntityDimensions {
            width: entity_type.dimension[0],
            height: entity_type.dimension[1],
            eye_height: entity_type.eye_height,
        },
    )) {
        return false;
    }
    true
}

/// 对应原版 `Entity.blocksBuilding` 字段。
///
/// 原版在 `LivingEntity`（`LivingEntity.java:327`）、`FallingBlockEntity`、`PrimedTnt`、
/// `EndCrystal`、`AbstractMinecart`、`AbstractBoat` 的构造里把它置为 `true`；
/// `ArmorStand` 则是 `blocksBuilding = !isMarker()`（`ArmorStand.java:626`）。
/// 只有它为 `true` 的实体才会在 `EntityGetter.isUnobstructed` 里挡住生成。
fn blocks_building(entity: &dyn EntityBase) -> bool {
    // 展示架是 marker 时不挡建造，也就不挡生成。
    if let Some(armor_stand) = entity.cast_any().downcast_ref::<ArmorStandEntity>() {
        return !armor_stand.is_marker();
    }
    // 所有 LivingEntity（含玩家与全部生物）都挡住生成 —— 这正是原版阻止同群怪物
    // 叠在同一格的机制。
    if entity.get_living_entity().is_some() {
        return true;
    }
    if entity.cast_any().is::<BoatEntity>() || entity.cast_any().is::<MinecartEntity>() {
        return true;
    }
    let entity_type = entity.get_entity().entity_type;
    entity_type == &EntityType::FALLING_BLOCK
        || entity_type == &EntityType::TNT
        || entity_type == &EntityType::END_CRYSTAL
}

/// 原版 `NaturalSpawner.isValidPositionForMob`（`NaturalSpawner.java:241`）末尾调用的
/// `Mob.checkSpawnObstruction`（`Mob.java:847`）中的 `level.isUnobstructed(this)` 部分，
/// 实现见 `EntityGetter.isUnobstructed`（`EntityGetter.java:40`）：包围盒内只要存在一个
/// 未被移除、非旁观者、且 `blocksBuilding` 为真的实体，就判定该位置被占用。
///
/// 需要特别说明的是：原版 `isValidSpawnPostitionForType` 末尾那个
/// `level.noCollision(getSpawnAABB(..))` **并不能**阻止生物互相重叠 —— 它的实体部分走
/// `EntitySelector.CAN_BE_COLLIDED_WITH`（`EntitySelector.java:31`），最终调用
/// `Entity.canBeCollidedWith(null)`，而该方法在 `Entity.java:2270` 默认返回 `false`，
/// 只有船 / 潜影贝 / 快乐恶魂重写过。所以同群怪物不叠在一格，靠的完全是这里的
/// `isUnobstructed`。
///
/// `pending` 是本批次已生成、但尚未由 `tick.rs` 写入世界的同群成员。原版是边生成边
/// `addFreshEntityWithPassengers`，后生成的成员立刻就能"看见"先生成的；Pumpkin 走批量
/// 缓冲，因此必须把缓冲区一并纳入判定，否则同一 pack 内部依旧会叠在一起。
///
/// 注意：`checkSpawnObstruction` 的另一半 `!level.containsAnyLiquid(bb)` 未在此实现 ——
/// 原版有大量水生 / 岩浆生物（`WaterAnimal`、`Drowned`、`Guardian`、`Axolotl`、`Strider`
/// 等）重写掉了这一半，按类逐个对齐超出本次修复范围；且 Pumpkin 的
/// `is_valid_empty_spawn_block` 已经拒绝液体方块，陆生生物不受影响。
#[must_use]
pub fn is_unobstructed_for_spawn(
    world: &World,
    entity: &dyn EntityBase,
    pending: &[Arc<dyn EntityBase>],
) -> bool {
    let bounding_box = entity.get_entity().bounding_box.load();

    // getEntities(source, bb) 自带 EntitySelector.NO_SPECTATORS，故排除旁观者。
    let blocks_spawn = |candidate: &dyn EntityBase| {
        let other = candidate.get_entity();
        if other.is_removed() {
            return false;
        }
        if candidate
            .get_player()
            .is_some_and(|player| player.gamemode.load() == GameMode::Spectator)
        {
            return false;
        }
        blocks_building(candidate) && other.bounding_box.load().intersects(&bounding_box)
    };

    // 先查同批次缓冲（廉价），再查世界中已存在的实体。
    if pending.iter().any(|other| blocks_spawn(other.as_ref())) {
        return false;
    }
    !world
        .get_all_at_box(&bounding_box)
        .iter()
        .any(|other| blocks_spawn(other.as_ref()))
}

pub fn is_spawn_position_ok(
    world: &Arc<World>,
    block_pos: &BlockPos,
    entity_type: &'static EntityType,
) -> bool {
    match entity_type.spawn_restriction.location {
        SpawnLocation::InLava => world.get_fluid(block_pos).has_tag(&MINECRAFT_LAVA),
        SpawnLocation::InWater => {
            let above_state = world.get_block_state(&block_pos.up());
            world.get_fluid(block_pos).has_tag(&MINECRAFT_WATER)
                && !is_redstone_conductor(above_state)
        }
        SpawnLocation::OnGround => {
            let down = world.get_block_state(&block_pos.down());
            let up = world.get_block_state(&block_pos.up());
            let cur = world.get_block_state(block_pos);
            // TODO: blockState.allowsSpawning
            let is_valid_spawn_below =
                down.is_side_solid(BlockDirection::Up) && down.luminance < 14;

            if is_valid_spawn_below {
                is_valid_empty_spawn_block(cur, entity_type)
                    && is_valid_empty_spawn_block(up, entity_type)
            } else {
                false
            }
        }
        SpawnLocation::Unrestricted => true,
    }
}

/// Cache-based version of `is_spawn_position_ok` used during world generation.
pub fn is_spawn_position_ok_cache(
    cache: &dyn GenerationCache,
    block_pos: &BlockPos,
    entity_type: &'static EntityType,
) -> bool {
    let pos_vec = block_pos.0;
    let state = GenerationCache::get_block_state(cache, &pos_vec).to_state();

    match entity_type.spawn_restriction.location {
        SpawnLocation::InLava => {
            // During generation, we check the block state's liquid property and tag
            state.is_liquid() && Block::from_state_id(state.id).has_tag(&MINECRAFT_LAVA)
        }
        SpawnLocation::InWater => {
            let above_pos = block_pos.up().0;
            let above_state = GenerationCache::get_block_state(cache, &above_pos).to_state();

            state.is_liquid()
                && Block::from_state_id(state.id).has_tag(&MINECRAFT_WATER)
                && !is_redstone_conductor(above_state)
        }
        SpawnLocation::OnGround => {
            let down_pos = block_pos.down().0;
            let up_pos = block_pos.up().0;

            let down = GenerationCache::get_block_state(cache, &down_pos).to_state();
            let up = GenerationCache::get_block_state(cache, &up_pos).to_state();

            // Logic: solid surface below and low enough light level (if applicable in generation)
            let is_valid_spawn_below =
                down.is_side_solid(BlockDirection::Up) && down.luminance < 14;

            if is_valid_spawn_below {
                is_valid_empty_spawn_block(state, entity_type)
                    && is_valid_empty_spawn_block(up, entity_type)
            } else {
                false
            }
        }
        SpawnLocation::Unrestricted => true,
    }
}

/// Cache-based version of `adjust_spawn_position` used during world generation.
pub fn adjust_spawn_position_cache(
    cache: &dyn GenerationCache,
    pos: BlockPos,
    entity_type: &'static EntityType,
) -> BlockPos {
    if matches!(
        entity_type.spawn_restriction.location,
        SpawnLocation::OnGround
    ) {
        let below = pos.down();
        let state = GenerationCache::get_block_state(cache, &below.0).to_state();

        if !state.is_full_cube() && !state.is_liquid() {
            return below;
        }
    }
    pos
}

pub fn adjust_spawn_position(
    world: &World,
    pos: BlockPos,
    entity_type: &'static EntityType,
) -> BlockPos {
    if matches!(
        entity_type.spawn_restriction.location,
        SpawnLocation::OnGround
    ) {
        let below = pos.down();
        let state = world.get_block_state(&below);
        // Approximation of isPathfindable(LAND)
        if !state.is_full_cube() && !state.is_liquid() {
            return below;
        }
    }
    pos
}

/// Vanilla `BlockState.isSignalSource` for the blocks whose non-full collision
/// shapes would otherwise let a mob spawn inside them.
#[must_use]
fn is_signal_source(state: &BlockState) -> bool {
    let block = Block::from_state_id(state.id);

    block.has_tag(&MINECRAFT_BUTTONS)
        || block.has_tag(&MINECRAFT_PRESSURE_PLATES)
        || matches!(
            block.id,
            BlockId::REPEATER
                | BlockId::COMPARATOR
                | BlockId::DAYLIGHT_DETECTOR
                | BlockId::LEVER
                | BlockId::SCULK_SENSOR
                | BlockId::CALIBRATED_SCULK_SENSOR
                | BlockId::LIGHTNING_ROD
                | BlockId::LECTERN
                | BlockId::REDSTONE_BLOCK
                | BlockId::JUKEBOX
                | BlockId::REDSTONE_TORCH
                | BlockId::REDSTONE_WALL_TORCH
                | BlockId::TRAPPED_CHEST
                | BlockId::OBSERVER
                | BlockId::TARGET
                | BlockId::REDSTONE_WIRE
                | BlockId::TRIPWIRE_HOOK
                | BlockId::DETECTOR_RAIL
        )
}

/// Vanilla `EntityType.isBlockDangerous` immunity tags.
#[must_use]
fn is_immune_to_block(entity_type: &EntityType, block: &Block) -> bool {
    let id = entity_type.id;
    (id == EntityType::FOX.id && block.has_tag(&MINECRAFT_FOX_IMMUNE_TO))
        || (id == EntityType::POLAR_BEAR.id && block.has_tag(&MINECRAFT_POLAR_BEAR_IMMUNE_TO))
        || (id == EntityType::SNOW_GOLEM.id && block.has_tag(&MINECRAFT_SNOW_GOLEM_IMMUNE_TO))
        || (id == EntityType::STRAY.id && block.has_tag(&MINECRAFT_STRAY_IMMUNE_TO))
        || (id == EntityType::WITHER.id && block.has_tag(&MINECRAFT_WITHER_IMMUNE_TO))
        || (id == EntityType::WITHER_SKELETON.id
            && block.has_tag(&MINECRAFT_WITHER_SKELETON_IMMUNE_TO))
}

/// Vanilla `NodeEvaluator.isBurningBlock`.
#[must_use]
fn is_burning_block(state: &BlockState, block: &Block) -> bool {
    block.has_tag(&MINECRAFT_FIRE)
        || matches!(
            block.id,
            BlockId::LAVA | BlockId::MAGMA_BLOCK | BlockId::LAVA_CAULDRON
        )
        || (matches!(block.id, BlockId::CAMPFIRE | BlockId::SOUL_CAMPFIRE) && state.luminance > 0)
}

/// Vanilla `EntityType.isBlockDangerous`.
#[must_use]
fn is_block_dangerous(state: &BlockState, entity_type: &EntityType) -> bool {
    let block = Block::from_state_id(state.id);
    if is_immune_to_block(entity_type, block) {
        return false;
    }

    if !entity_type.fire_immune && is_burning_block(state, block) {
        return true;
    }

    matches!(
        block.id,
        BlockId::WITHER_ROSE | BlockId::SWEET_BERRY_BUSH | BlockId::CACTUS | BlockId::POWDER_SNOW
    )
}

/// Vanilla `NaturalSpawner.isValidEmptySpawnBlock`.
#[must_use]
pub fn is_valid_empty_spawn_block(state: &BlockState, entity_type: &EntityType) -> bool {
    if state.is_full_cube() {
        return false;
    }
    if is_signal_source(state) {
        return false;
    }
    if state.is_liquid() {
        return false;
    }
    if Block::from_state_id(state.id).has_tag(&MINECRAFT_PREVENT_MOB_SPAWNING_INSIDE) {
        return false;
    }

    !is_block_dangerous(state, entity_type)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::natural_spawner as public_api;

    // Compile-time assertions that the public paths and signatures survived the
    // module split (re-exported through `crate::world::natural_spawner`).
    const _: fn(&World, Vector2<i32>) -> bool = public_api::any_player_close_enough_for_spawning;
    const _: fn(&World, Vector2<i32>) -> bool = public_api::is_natural_spawn_candidate;
    const _: fn(&Vector3<f64>, &[Vector3<f64>]) -> f64 = public_api::get_nearest_player;
    const _: fn(&World, &BlockPos, f64, &Vector2<i32>, Vector3<f64>) -> bool =
        public_api::is_right_distance_to_player_and_spawn_point;
    const _: fn(&Arc<World>, &'static MobCategory, &BlockPos) -> Option<&'static Spawner> =
        public_api::get_random_spawn_mob_at;
    const _: fn(
        &Arc<World>,
        &BlockPos,
        &'static MobCategory,
        &'static EntityType,
        f64,
        bool,
    ) -> bool = public_api::is_valid_spawn_position_for_type;
    const _: fn(&Arc<World>, &BlockPos, &'static EntityType) -> bool =
        public_api::is_spawn_position_ok;
    const _: fn(&dyn GenerationCache, &BlockPos, &'static EntityType) -> bool =
        public_api::is_spawn_position_ok_cache;
    const _: fn(&dyn GenerationCache, BlockPos, &'static EntityType) -> BlockPos =
        public_api::adjust_spawn_position_cache;
    const _: fn(&World, BlockPos, &'static EntityType) -> BlockPos =
        public_api::adjust_spawn_position;
    const _: fn(&BlockState, &EntityType) -> bool = public_api::is_valid_empty_spawn_block;
    const _: fn(&World, &dyn EntityBase, &[Arc<dyn EntityBase>]) -> bool =
        public_api::is_unobstructed_for_spawn;
    const _: i32 = public_api::NATURAL_SPAWN_CHUNK_RANGE;
    const _: f64 = public_api::SPAWN_DISTANCE_BLOCK_SQ;

    /// 现场报告里约 10 只 husk 叠在同一格。`is_unobstructed_for_spawn` 靠包围盒相交来
    /// 拒绝这种位置，所以这里固定住它依赖的几何前提：同一格必然相交（会被拒绝），
    /// 相邻格互不相交（原版散布后应有的样子）。
    #[test]
    fn husk_spawn_boxes_overlap_on_same_block_but_not_one_block_apart() {
        let size = EntityDimensions {
            width: EntityType::HUSK.dimension[0],
            height: EntityType::HUSK.dimension[1],
            eye_height: EntityType::HUSK.eye_height,
        };
        let at = |x: f64, z: f64| BoundingBox::new_from_pos(x + 0.5, 64.0, z + 0.5, &size);

        assert!(at(10.0, 20.0).intersects(&at(10.0, 20.0)));
        assert!(!at(10.0, 20.0).intersects(&at(11.0, 20.0)));
        assert!(!at(10.0, 20.0).intersects(&at(10.0, 21.0)));
    }

    #[test]
    fn redstone_conductors_are_not_limited_to_full_cubes() {
        let soul_sand = Block::SOUL_SAND.default_state;
        assert!(is_redstone_conductor(soul_sand));
        assert!(!soul_sand.is_full_cube());
        assert!(!is_redstone_conductor(Block::REDSTONE_BLOCK.default_state));
    }

    #[test]
    fn empty_spawn_blocks_reject_signal_sources() {
        let lever = Block::LEVER.default_state;
        assert!(!lever.is_full_cube());
        assert!(is_signal_source(lever));
        assert!(!is_valid_empty_spawn_block(lever, &EntityType::ZOMBIE));
    }

    #[test]
    fn dangerous_spawn_blocks_honor_entity_immunities() {
        assert!(is_block_dangerous(
            Block::FIRE.default_state,
            &EntityType::ZOMBIE,
        ));
        assert!(!is_block_dangerous(
            Block::FIRE.default_state,
            &EntityType::BLAZE,
        ));
        assert!(is_block_dangerous(
            Block::SWEET_BERRY_BUSH.default_state,
            &EntityType::ZOMBIE,
        ));
        assert!(!is_block_dangerous(
            Block::SWEET_BERRY_BUSH.default_state,
            &EntityType::FOX,
        ));
        assert!(is_block_dangerous(
            Block::POWDER_SNOW.default_state,
            &EntityType::ZOMBIE,
        ));
        assert!(!is_block_dangerous(
            Block::POWDER_SNOW.default_state,
            &EntityType::POLAR_BEAR,
        ));
    }

    #[test]
    fn packs_can_cross_into_an_active_neighboring_chunk() {
        let origin = Vector2::new(0, 0);
        let neighbor = Vector2::new(1, 0);
        let mut active_chunks = FxHashSet::default();
        active_chunks.insert(neighbor);

        assert!(can_spawn_entities_in_chunk(origin, origin, &active_chunks,));
        assert!(can_spawn_entities_in_chunk(
            origin,
            neighbor,
            &active_chunks,
        ));
        assert!(!can_spawn_entities_in_chunk(
            origin,
            Vector2::new(2, 0),
            &active_chunks,
        ));
    }

    #[test]
    fn generated_biome_spawner_weights_are_preserved() {
        let zombie = Biome::BADLANDS
            .spawners
            .monster
            .iter()
            .find(|spawner| spawner.r#type == "minecraft:zombie")
            .expect("badlands has a zombie spawner");
        assert_eq!(zombie.weight, 95);
    }

    #[test]
    fn pack_spawner_must_exist_in_the_destination_biome_pool() {
        let zombie = Biome::BADLANDS
            .spawners
            .monster
            .iter()
            .find(|spawner| spawner.r#type == "minecraft:zombie")
            .expect("badlands has a zombie spawner");

        assert!(spawner_is_in_biome_pool(
            &Biome::BADLANDS,
            &MobCategory::MONSTER,
            zombie,
        ));
        assert!(!spawner_is_in_biome_pool(
            &Biome::WARPED_FOREST,
            &MobCategory::MONSTER,
            zombie,
        ));
    }

    #[test]
    fn selects_biome_spawners_by_weight_boundaries() {
        let spawners = [
            Spawner {
                r#type: "minecraft:first",
                min_count: 1,
                max_count: 1,
                weight: 2,
            },
            Spawner {
                r#type: "minecraft:second",
                min_count: 1,
                max_count: 1,
                weight: 3,
            },
            Spawner {
                r#type: "minecraft:third",
                min_count: 1,
                max_count: 1,
                weight: 1,
            },
        ];

        assert_eq!(
            select_weighted_spawner(&spawners, 0).map(|spawner| spawner.r#type),
            Some("minecraft:first")
        );
        assert_eq!(
            select_weighted_spawner(&spawners, 1).map(|spawner| spawner.r#type),
            Some("minecraft:first")
        );
        assert_eq!(
            select_weighted_spawner(&spawners, 2).map(|spawner| spawner.r#type),
            Some("minecraft:second")
        );
        assert_eq!(
            select_weighted_spawner(&spawners, 4).map(|spawner| spawner.r#type),
            Some("minecraft:second")
        );
        assert_eq!(
            select_weighted_spawner(&spawners, 5).map(|spawner| spawner.r#type),
            Some("minecraft:third")
        );
        assert!(select_weighted_spawner(&spawners, 6).is_none());
    }

    #[test]
    fn ignores_zero_weight_biome_spawners() {
        let spawners = [
            Spawner {
                r#type: "minecraft:ignored",
                min_count: 1,
                max_count: 1,
                weight: 0,
            },
            Spawner {
                r#type: "minecraft:selected",
                min_count: 1,
                max_count: 1,
                weight: 1,
            },
        ];

        assert_eq!(
            select_weighted_spawner(&spawners, 0).map(|spawner| spawner.r#type),
            Some("minecraft:selected")
        );
    }

    #[test]
    fn nearest_player_distance_is_the_squared_minimum() {
        let pos = Vector3::new(0.0, 0.0, 0.0);
        let players = [Vector3::new(3.0, 4.0, 0.0), Vector3::new(10.0, 0.0, 0.0)];
        assert!((get_nearest_player(&pos, &players) - 25.0).abs() < 1e-9);
        assert_eq!(get_nearest_player(&pos, &[]), f64::MAX);
    }
}
