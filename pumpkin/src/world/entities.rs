use crate::entity::{Entity, EntityBase, item::ItemEntity};
use crate::world::World;
use crate::world::chunker::{get_view_distance, is_within_view_distance};
use pumpkin_data::entity::EntityType;
use pumpkin_data::item_stack::ItemStack;
use pumpkin_nbt::compound::NbtCompound;
use pumpkin_protocol::bedrock::client::remove_actor::CRemoveActor;
use pumpkin_protocol::codec::var_long::VarLong;
use pumpkin_protocol::java::client::play::CRemoveEntities;
use pumpkin_util::Difficulty;
use pumpkin_util::math::{
    boundingbox::BoundingBox, position::BlockPos, vector2::Vector2, vector3::Vector3,
};
use pumpkin_util::random::{RandomImpl, get_seed, xoroshiro128::Xoroshiro};
use pumpkin_world::chunk::io::Dirtiable;
use pumpkin_world::inventory::Inventory;
use rand::RngExt;
use rustc_hash::FxHashSet;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::debug;

impl World {
    /// Serializes a live entity into its current chunk's entity data. The live
    /// entity list is the source of truth while a chunk is loaded (its saved NBT
    /// is consumed on load), so this simply appends the entity to the chunk it is
    /// currently in; the chunk is rewritten from scratch every unload cycle, so
    /// there is nothing stale to deduplicate.
    pub(super) async fn save_entity(&self, entity: &Arc<dyn EntityBase>) {
        let current_chunk = entity.get_entity().block_pos.load().chunk_position();
        let mut nbt = NbtCompound::new();
        entity.write_nbt(&mut nbt).await;
        let chunk = self.level.get_entity_chunk(current_chunk).await;
        chunk.data.lock().await.push(nbt);
        chunk.mark_dirty(true);
    }

    /// Gets an entity by an entity id
    pub fn get_entity_by_id(&self, id: i32) -> Option<Arc<dyn EntityBase>> {
        if let Some(entity) = self.entities.get_by_id(id) {
            return Some(entity);
        }
        for player in self.players.load().iter() {
            if player.get_entity().entity_id == id {
                return Some(player.clone() as Arc<dyn EntityBase>);
            }
        }
        None
    }

    // Gets all entities at a Box
    pub fn get_all_at_box(&self, aabb: &BoundingBox) -> Vec<Arc<dyn EntityBase>> {
        let entities_guard = self.entities.load();
        let players_guard = self.players.load();

        entities_guard
            .iter()
            .map(|e| e.clone() as Arc<dyn EntityBase>)
            .chain(
                players_guard
                    .iter()
                    .map(|p| p.clone() as Arc<dyn EntityBase>),
            )
            .filter(|entity| entity.get_entity().bounding_box.load().intersects(aabb))
            .collect()
    }

    // Gets all non Player entities at a Box
    pub fn get_entities_at_box(&self, aabb: &BoundingBox) -> Vec<Arc<dyn EntityBase>> {
        self.entities
            .load()
            .iter()
            .filter(|entity| entity.get_entity().bounding_box.load().intersects(aabb))
            .cloned()
            .collect()
    }

    /// Retrieves an entity by their unique UUID.
    ///
    /// This function searches the world's entities for one with the specified UUID.
    /// If found, it returns an `Arc<dyn EntityBase>` reference to that entity. Otherwise, it returns `None`.
    ///
    /// # Arguments
    ///
    /// * `id`: The UUID of the entity to retrieve.
    ///
    /// # Returns
    ///
    /// An `Option<Arc<dyn EntityBase>>` containing the player if found, or `None` if not.
    pub fn get_entity_by_uuid(&self, id: uuid::Uuid) -> Option<Arc<dyn EntityBase>> {
        self.entities.get_by_uuid(id)
    }

    pub fn get_nearby_entities(
        &self,
        pos: Vector3<f64>,
        radius: f64,
    ) -> HashMap<uuid::Uuid, Arc<dyn EntityBase>> {
        let radius_squared = radius.powi(2);

        self.entities
            .load()
            .iter()
            .filter_map(|entity| {
                let entity_pos = entity.get_entity().pos.load();
                (entity_pos.squared_distance_to_vec(&pos) <= radius_squared)
                    .then(|| (entity.get_entity().entity_uuid, entity.clone()))
            })
            .collect()
    }

    /// Gets the closest entity to a position, with optional filtering by entity type.
    ///
    /// # Arguments
    ///
    /// * `pos` - The position to search around.
    /// * `radius` - The radius to search within.
    /// * `entity_types` - Optional array of entity types to filter by. If None, all entity types are included.
    ///
    /// # Returns
    ///
    /// The closest entity that matches the filter criteria, or None if no entities are found.
    pub fn get_closest_entity(
        &self,
        pos: Vector3<f64>,
        radius: f64,
        entity_types: Option<&[&'static EntityType]>,
    ) -> Option<Arc<dyn EntityBase>> {
        // Get regular entities
        let entities = self.get_nearby_entities(pos, radius);

        // Filter by entity type if specified
        let filtered_entities = if let Some(types) = entity_types {
            entities
                .into_iter()
                .filter(|(_, entity)| {
                    let entity_type = entity.get_entity().entity_type;
                    types.contains(&entity_type)
                })
                .collect::<HashMap<_, _>>()
        } else {
            entities
        };

        // Find the closest entity
        filtered_entities
            .iter()
            .min_by(|a, b| {
                a.1.get_entity()
                    .pos
                    .load()
                    .squared_distance_to_vec(&pos)
                    .partial_cmp(&b.1.get_entity().pos.load().squared_distance_to_vec(&pos))
                    .unwrap()
            })
            .map(|p| p.1.clone())
    }

    /// Adds entities to the provided [`Vec`] that satisfy a particular condition and are
    /// present in the provided [`BoundingBox`].
    ///
    /// # Arguments
    ///
    /// * `list`: The `Vec` to add to.
    /// * `max_list_capacity`: The maximum capacity of `list` for adding entities. If this limit is reached, no more
    ///   entities will be added to the list. If `list` already reaches this limit, nothing happens.
    /// * `bounding_box`: The bounding box to filter any added entities.
    /// * `predicate`: A predicate function, which has to be `true` for an entity to be added to the list.
    pub fn extend_entities_in_box_where(
        &self,
        list: &mut Vec<Arc<dyn EntityBase>>,
        max_list_capacity: usize,
        bounding_box: BoundingBox,
        predicate: impl Fn(&dyn EntityBase) -> bool,
    ) {
        self.extend_entities_where(list, max_list_capacity, |e| {
            bounding_box.intersects(&e.get_entity().bounding_box.load()) && predicate(e)
        });
    }

    /// Adds entities to the provided [`Vec`] that satisfy a particular condition.
    ///
    /// # Arguments
    ///
    /// * `list`: The `Vec` to add to.
    /// * `max_list_capacity`: The maximum capacity of `list` for adding entities. If this limit is reached, no more
    ///   entities will be added to the list. If `list` already reaches this limit, nothing happens.
    /// * `predicate`: A predicate function, which has to be `true` for an entity to be added to the list.
    pub fn extend_entities_where(
        &self,
        list: &mut Vec<Arc<dyn EntityBase>>,
        max_list_capacity: usize,
        predicate: impl Fn(&dyn EntityBase) -> bool,
    ) {
        if list.len() >= max_list_capacity {
            return;
        }
        // Loop the players.
        for player in self.players.load().iter() {
            if !predicate(player.as_ref()) {
                continue;
            }
            // We add the player to the list.
            list.push(player.clone());
            // Check if the list is too big.
            if list.len() > max_list_capacity {
                return;
            }
        }
        // Same with entities.
        for entity in self.entities.load().iter() {
            if !predicate(entity.as_ref()) {
                continue;
            }
            list.push(entity.clone());
            if list.len() > max_list_capacity {
                return;
            }
            // TODO: Implement ender dragon handling
        }
    }

    pub fn spawn_entity_non_save(&self, entity: &Arc<dyn EntityBase>) {
        let _base_entity = entity.get_entity();
        self.broadcast_entity_spawn(entity);
        if self.entities.add(entity.clone()) {
            self.spawn_state.load().add_entity(self, entity.as_ref());
        }
    }

    pub async fn spawn_entity(&self, entity: Arc<dyn EntityBase>) {
        // Vanilla tracking order: spawn packet first, then entity data.
        // Metadata (e.g. ItemEntity DATA_ITEM / ItemStack) sent before the client
        // knows the entity id is dropped — drops would render as empty/invisible.
        // Equipment is attached in try_enqueue_spawn_packet after CSpawnEntity.
        self.broadcast_entity_spawn(&entity);
        entity.init_data_tracker().await;
        self.add_entity_silent(entity).await;
    }

    /// Adds a natural spawn already accounted for by `SpawnState::after_spawn`.
    pub(super) async fn spawn_natural_entity(&self, entity: Arc<dyn EntityBase>) {
        self.broadcast_entity_spawn(&entity);
        entity.init_data_tracker().await;
        let _ = self.entities.add(entity);
    }

    pub fn broadcast_entity_spawn(&self, entity: &Arc<dyn EntityBase>) {
        let base_entity = entity.get_entity();
        let chunk_pos = base_entity.chunk_pos.load();

        let players = self.players.load();
        for player in players.iter() {
            let center = player.get_entity().chunk_pos.load();
            let view_distance = get_view_distance(player).get() as i32;

            if is_within_view_distance(chunk_pos, center, view_distance) {
                player.client.try_enqueue_spawn_packet(entity);
            }
        }
    }

    #[allow(clippy::unused_async)]
    pub async fn add_entity_silent(&self, entity: Arc<dyn EntityBase>) {
        // Guard against duplicate UUID (vanilla EntityLookup.add). Can happen
        // when chunk entity data is loaded while the entity is already live.
        if !self.entities.add(entity.clone()) {
            return;
        }

        // The entity stays live-only: it is written to its chunk's saved data on
        // unload (see `save_entity`), never at spawn, so it can't be both live and
        // serialized at once (which would double it on the next reload).
        self.spawn_state.load().add_entity(self, entity.as_ref());
    }

    pub async fn remove_entity(&self, entity: &dyn EntityBase) {
        // Sever mount links so vehicle/passenger Arc pairs (chicken jockeys,
        // ridden mobs) can actually drop instead of keeping each other alive.
        {
            let base = entity.get_entity();
            let vehicle = base.vehicle.lock().await.take();
            if let Some(vehicle) = vehicle {
                vehicle.get_entity().remove_passenger(base.entity_id).await;
            }
            let passengers: Vec<_> = base.passengers.lock().await.drain(..).collect();
            for passenger in passengers {
                *passenger.get_entity().vehicle.lock().await = None;
            }
        }
        let base_entity = entity.get_entity();
        // Ensure concurrent tick/damage paths see the entity as gone even if
        // callers forgot to call `Entity::mark_removed` first.
        base_entity.mark_removed(crate::entity::RemovalReason::Discarded);

        // 断开拴绳。对齐原版 `Leashable.tickLeash` 里「实体不再可交互就解链」那条分支
        // （`Leashable.java:161-166`：`!entity.canInteractWithLevel()` 时按 `ENTITY_DROPS`
        // gamerule 走 `dropLeash()` 或 `removeLeash()`；`canInteractWithLevel` 见
        // `Entity.java:389-391`，被移除的实体必然为假）。原版靠被拴实体自己每 tick
        // 复查来触发，Pumpkin 的实体一离开 `EntityLookup` 就不再 tick，`tick_leash`
        // 永远不会再跑，被拴这一头持有的 holder `Arc<dyn EntityBase>` 也就永远断不掉。
        //
        // Pumpkin 没有 `ENTITY_DROPS` gamerule，改用 `RemovalReason::should_destroy()`
        // 近似：死亡/丢弃掉拴绳，卸载/换维度只解链——后者与原版
        // `Entity.removeAfterChangingDimensions`（`Entity.java:3034-3040`，那里同样只调
        // `removeLeash()`）一致。
        if base_entity
            .removal_reason
            .load()
            .is_some_and(|reason| reason.should_destroy())
        {
            base_entity.drop_leash().await;
        } else {
            base_entity.unleash().await;
        }

        // 抹掉 AI 状态。`MobEntity.target` 和 15 个 goal 字段持的都是
        // `Arc<dyn EntityBase>`，两只互相锁定的 mob 就是一个谁也释放不掉的强引用环。
        // 对齐原版 `Mob.removeFreeWill()`（`Mob.java:1424-1427`），细节见
        // `entity/mob/mod.rs` 里 `remove_free_will` 的注释。
        //
        // mob 视图必须从注册表里那份 `Arc` 上取，不能直接用参数 `entity`：
        // `Entity::remove`（`entity/mod.rs:2616`）传进来的是 `&Entity`，而
        // `impl EntityBase for Entity` 没有覆写 `get_mob`（只有 `mob/entity_base.rs:13`
        // 的 blanket impl 覆写了），于是自动消散（`mob/despawn.rs:61`）、蠹虫钻石头
        // （`ai/goal/silverfish_merge.rs:104`）这些走 `Entity::remove` 的路径在参数上只
        // 能拿到 `None`。注册表里存的是具体 mob 类型的 `Arc`，两种调用形态都能命中。
        let registered = self.entities.get_by_uuid(base_entity.entity_uuid);
        let mob_source = registered.as_deref().unwrap_or(entity);
        if let Some(mob) = mob_source.get_mob() {
            crate::entity::mob::remove_free_will(mob).await;
        }

        // O(1) remove — only adjust spawn caps if the entity was actually present
        // (prevents double-remove under-counting mob caps).
        if self.entities.remove(entity).is_some() {
            self.spawn_state.load().remove_entity(self, entity);
        } else {
            debug!(
                entity_id = base_entity.entity_id,
                entity_uuid = %base_entity.entity_uuid,
                entity_type = base_entity.entity_type.resource_name,
                "remove_entity: entity was not in world list (already removed)"
            );
        }

        let chunk_pos = base_entity.chunk_pos.load();
        self.broadcast_to_chunk_editioned_sync(
            chunk_pos,
            &CRemoveEntities::new(&[base_entity.entity_id.into()]),
            &CRemoveActor::new(VarLong(base_entity.entity_id as i64)),
        );
    }

    pub async fn remove_entities_in_chunks(&self, chunks: &[Vector2<i32>]) {
        let chunks_set: FxHashSet<_> = chunks.iter().copied().collect();
        let entities_to_remove = self.entities.drain_if(|entity| {
            let base_entity = entity.get_entity();
            // 按 `chunk_pos` 匹配。这是唯一会把实体从 `EntityLookup` 批量摘掉的
            // 地方，依赖「`chunk_pos` 永远和实时 `pos` 一致」这条不变量——该不变量
            // 由 `Entity::set_pos` 维护，所有位置写入都必须走它（对齐原版
            // `Entity.setPosRaw`，见 `entity/projectile/*`、`entity/vehicle/minecart.rs`）。
            // 一旦有人绕过 `set_pos` 直接写 `pos`，那个实体的缓存值就会永久失效，
            // 它真正所在的区块卸载时匹配不上，于是永远留在 `EntityLookup` 里；
            // 又因为 `Entity.world` 是强引用（`entity/mod.rs:720`）而 `EntityLookup`
            // 持有 `Arc<dyn EntityBase>`（`world/entity_lookup.rs:57-58`），
            // 形成强引用环，实体连同它引用的一切（AI target、载具、玩家）都不再释放。
            if chunks_set.contains(&base_entity.chunk_pos.load()) {
                base_entity.mark_removed(crate::entity::RemovalReason::UnloadedToChunk);
                true
            } else {
                false
            }
        });

        for entity in entities_to_remove {
            self.save_entity(&entity).await;
            self.spawn_state.load().remove_entity(self, entity.as_ref());

            // 区块卸载同样要断掉强引用。原版这条路径走的是 `setRemoved(UNLOADED_TO_CHUNK)`
            // 而不是 `remove()`（`PersistentEntitySectionManager.java:241`），所以原版
            // 在这里既不清 brain 记忆也不解链——它不需要：实体对象整体交给 GC，环会被
            // 一起回收，拴绳则由 NBT 里的 `LeashData` 在重新加载时恢复。
            //
            // Pumpkin 两点都不成立：`Arc` 环不会被回收，而拴绳压根不写 NBT，重新加载
            // 后本就不存在。所以这里必须显式断开，且必须放在 `save_entity` 之后——
            // AI 状态和拴绳都不参与序列化，清理不改变落盘内容，但顺序上先存后清最稳。
            // 只解链、不掉拴绳物品：`UnloadedToChunk` 的 `should_destroy()` 为假，对齐
            // 原版 `Leashable.java:165` 在 `ENTITY_DROPS` 关闭时选 `removeLeash()`。
            entity.get_entity().unleash().await;
            if let Some(mob) = entity.get_mob() {
                crate::entity::mob::remove_free_will(mob).await;
            }
        }

        for chunk_pos in &chunks_set {
            self.block_entities.remove(chunk_pos);
        }
    }

    pub async fn drop_stack(self: &Arc<Self>, pos: &BlockPos, stack: ItemStack) {
        let height = EntityType::ITEM.dimension[1] / 2.0;
        let spawn_pos = {
            let mut r = rand::rng();
            Vector3::new(
                f64::from(pos.0.x) + 0.5 + r.random_range(-0.25..0.25),
                f64::from(pos.0.y) + 0.5 + r.random_range(-0.25..0.25) - f64::from(height),
                f64::from(pos.0.z) + 0.5 + r.random_range(-0.25..0.25),
            )
        };

        let entity = Entity::new(self.clone(), spawn_pos, &EntityType::ITEM);
        let item_entity = Arc::new(ItemEntity::new(entity, stack));
        self.spawn_entity(item_entity).await;
    }

    /* ItemScatterer.java */

    pub async fn scatter_inventory(
        self: &Arc<Self>,
        position: &BlockPos,
        inventory: &Arc<dyn Inventory>,
    ) {
        for i in 0..inventory.size() {
            self.scatter_stack(
                f64::from(position.0.x),
                f64::from(position.0.y),
                f64::from(position.0.z),
                inventory.remove_stack(i).await,
            )
            .await;
        }
    }

    pub async fn scatter_stack(self: &Arc<Self>, x: f64, y: f64, z: f64, mut stack: ItemStack) {
        const TRIANGULAR_DEVIATION: f64 = 0.114_850_001_711_398_36;

        const XZ_MODE: f64 = 0.0;
        const Y_MODE: f64 = 0.2;

        let width = f64::from(EntityType::ITEM.dimension[0]);
        let half_width = width / 2.0;
        let spawn_area = 1.0 - width;

        let mut rng = Xoroshiro::from_seed(get_seed());

        // TODO: Use world random here: world.random.nextDouble()
        let x = rng.next_f64().mul_add(spawn_area, x.floor()) + half_width;
        let y = rng.next_f64().mul_add(spawn_area, y.floor());
        let z = rng.next_f64().mul_add(spawn_area, z.floor()) + half_width;

        while !stack.is_empty() {
            let item = stack.split((rng.next_bounded_i32(21) + 10) as u8);
            let velocity = Vector3::new(
                rng.next_triangular(XZ_MODE, TRIANGULAR_DEVIATION),
                rng.next_triangular(Y_MODE, TRIANGULAR_DEVIATION),
                rng.next_triangular(XZ_MODE, TRIANGULAR_DEVIATION),
            );

            let entity = Entity::new(self.clone(), Vector3::new(x, y, z), &EntityType::ITEM);
            let entity = Arc::new(ItemEntity::new_with_velocity(entity, item, velocity, 10));
            self.spawn_entity(entity).await;
        }
    }
    /* End ItemScatterer.java */

    /// Returns whether monsters can be spawned in the world
    pub fn should_spawn_monsters(&self) -> bool {
        let level_data = self.level_info.load();
        level_data.game_rules.spawn_mobs
            && level_data.game_rules.spawn_monsters
            && level_data.difficulty != Difficulty::Peaceful
    }
}
