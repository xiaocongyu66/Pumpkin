//! `LivingEntity`：玩家、生物等所有活体实体的公共实现。
//!
//! 对应原版 `/root/Vanilla/src/net/minecraft/world/entity/LivingEntity.java`。
//! 该类本身 3820 行超出单文件上限，故按职责拆成下列子模块；子模块里全部是
//! `impl LivingEntity` / `impl EntityBase for LivingEntity` /
//! `impl NBTStorage for LivingEntity`，inherent impl 与 trait impl 不受模块路径
//! 影响，因此外部 `use` 无需改动。
//!
//! - [`damage`]：受伤主流程、护甲与附魔减免、不死图腾
//! - [`death`]：死亡、掉落物、经验球与统计
//! - [`effects`]：生命值、吸收、属性与状态效果
//! - [`entity_base`]：`EntityBase` 实现（tick、重力、访问器）
//! - [`equipment`]：装备槽、手持物与使用状态
//! - [`movement`]：移动、游泳、攀爬与坠落
//! - [`nbt`]：NBT 读写

mod damage;
mod death;
mod effects;
mod entity_base;
mod equipment;
mod movement;
mod nbt;

use super::Entity;
use crate::entity::attributes::AttributeInstance;
use crossbeam::atomic::AtomicCell;
use pumpkin_data::Block;
use pumpkin_data::attributes::Attributes;
use pumpkin_data::damage::DamageType;
use pumpkin_data::data_component_impl::EquipmentSlot;
use pumpkin_data::effect::StatusEffect;
use pumpkin_data::entity::EntityType;
use pumpkin_data::item_stack::ItemStack;
use pumpkin_data::potion::Effect;
use pumpkin_data::sound::{Sound, SoundCategory};
use pumpkin_inventory::build_equipment_slots;
use pumpkin_inventory::entity_equipment::EntityEquipment;
use pumpkin_util::Hand;
use pumpkin_util::math::position::BlockPos;
use pumpkin_util::math::vector3::Vector3;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8};
use std::sync::{Arc, RwLock};
use tokio::sync::Mutex;

/// Represents a living entity within the game world.
///
/// This struct encapsulates the core properties and behaviors of living entities, including players, mobs, and other creatures.
pub struct LivingEntity {
    /// The underlying entity object, providing basic entity information and functionality.
    pub entity: Entity,
    /// Tracks the remaining time until the entity can regenerate health.
    pub hurt_cooldown: AtomicI32,
    /// Stores the amount of damage the entity last received.
    pub last_damage_taken: AtomicCell<f32>,
    /// The current health level of the entity.
    pub health: AtomicCell<f32>,
    /// The current absorption (yellow hearts) on the entity.
    pub absorption: AtomicCell<f32>,
    pub item_use_time: AtomicI32,
    pub item_in_use: Mutex<Option<ItemStack>>,
    pub active_hand: Mutex<Option<Hand>>,
    pub death_time: AtomicU8,
    /// Indicates whether the entity is dead. (`on_death` called)
    pub dead: AtomicBool,
    /// The distance the entity has been falling.
    pub fall_distance: AtomicCell<f32>,
    pub active_effects: Mutex<HashMap<&'static StatusEffect, Effect>>,
    pub entity_equipment: Arc<Mutex<EntityEquipment>>,
    pub equipment_drop_chances: Arc<Mutex<HashMap<EquipmentSlot, f32>>>,
    pub movement_input: AtomicCell<Vector3<f64>>,
    /// Vanilla `LivingEntity.speed` — set by MoveControl/Navigator via `set_speed`.
    /// Travel uses this for mobs (players keep using the movement_speed attribute).
    pub living_speed: AtomicCell<f64>,
    pub equipment_slots: Arc<HashMap<usize, EquipmentSlot>>,

    pub jumping: AtomicBool,

    pub jumping_cooldown: AtomicU8,

    pub climbing: AtomicBool,

    /// The position where the entity was last climbing, used for death messages
    pub climbing_pos: AtomicCell<Option<BlockPos>>,

    /// The entity ID of the entity that last attacked this living entity.
    pub last_attacker_id: AtomicI32,
    /// The tick at which this entity was last attacked (entity age).
    pub last_attacked_time: AtomicI32,

    /// The entity ID of the entity this living entity last attacked.
    pub last_attacking_id: AtomicI32,
    /// The tick at which this entity last attacked something (entity age).
    pub last_attack_time: AtomicI32,

    water_movement_speed_multiplier: f32,
    livings_flags: AtomicU8,

    /// The attributes of the entity
    pub attributes: RwLock<HashMap<u8, AttributeInstance>>,
}

impl LivingEntity {
    const USING_ITEM_FLAG: u8 = 1;
    const OFF_HAND_ACTIVE_FLAG: u8 = 2;
    const ACTIVE_HAND_FLAGS: u8 = Self::USING_ITEM_FLAG | Self::OFF_HAND_ACTIVE_FLAG;
    // Only referenced by the metadata tests today.
    #[cfg_attr(not(test), expect(dead_code))]
    const USING_RIPTIDE_FLAG: u8 = 4;

    const PREVENT_AREA_FALL_DAMAGE_BLOCKS: [&'static Block; 4] = [
        &Block::COBWEB,
        &Block::LADDER,
        &Block::POWDER_SNOW,
        &Block::SLIME_BLOCK,
    ];

    fn hurt_sound_for_entity(entity_type: &'static EntityType) -> Sound {
        if let Some(sound) = entity_type.hurt_sound {
            return sound;
        }
        // Most living entities omit hurt_sound in entities.json (only a few undead
        // set it). Resolve `entity.<name>.hurt` from the sound registry so villagers
        // get the "hmm", wolves yelp, golems clang, etc.
        let key = format!("entity.{}.hurt", entity_type.resource_name);
        Sound::from_name(&key).unwrap_or(Sound::EntityGenericHurt)
    }

    fn sound_category_for_entity(entity_type: &'static EntityType) -> SoundCategory {
        use pumpkin_data::entity::MobCategory;
        if entity_type == &EntityType::PLAYER {
            SoundCategory::Players
        } else if entity_type.category == &MobCategory::MONSTER {
            SoundCategory::Hostile
        } else {
            SoundCategory::Neutral
        }
    }

    pub fn new(entity: Entity) -> Self {
        let water_movement_speed_multiplier = if entity.entity_type == &EntityType::POLAR_BEAR {
            0.98
        } else if entity.entity_type == &EntityType::SKELETON_HORSE {
            0.96
        } else {
            0.8
        };
        let mut max_health: f32 = 20.0; // Overridden by attribute base below
        Self {
            // Populate local attribute instances from the default registry and get initial vars
            attributes: {
                let mut m = std::collections::HashMap::new();

                for (attr, base) in entity.entity_type.attributes {
                    if attr.id == Attributes::MAX_HEALTH.id {
                        max_health = *base as f32;
                    }
                    m.insert(attr.id, AttributeInstance::new(*base));
                }
                std::sync::RwLock::new(m)
            },
            health: AtomicCell::new(max_health), // Initial health value from attributes
            entity,
            hurt_cooldown: AtomicI32::new(0),
            last_damage_taken: AtomicCell::new(0.0),
            absorption: AtomicCell::new(0.0),
            fall_distance: AtomicCell::new(0.0),
            death_time: AtomicU8::new(0),
            dead: AtomicBool::new(false),
            item_use_time: AtomicI32::new(0),
            item_in_use: Mutex::new(None),
            active_hand: Mutex::new(None),
            livings_flags: AtomicU8::new(0),
            active_effects: Mutex::new(HashMap::new()),
            entity_equipment: Arc::new(Mutex::new(EntityEquipment::new())),
            equipment_drop_chances: Arc::new(Mutex::new(HashMap::new())),
            equipment_slots: Arc::new(build_equipment_slots()),
            jumping: AtomicBool::new(false),
            jumping_cooldown: AtomicU8::new(0),
            climbing: AtomicBool::new(false),
            climbing_pos: AtomicCell::new(None),
            last_attacker_id: AtomicI32::new(0),
            last_attacked_time: AtomicI32::new(0),
            last_attacking_id: AtomicI32::new(0),
            last_attack_time: AtomicI32::new(0),
            movement_input: AtomicCell::new(Vector3::default()),
            living_speed: AtomicCell::new(0.0),
            water_movement_speed_multiplier,
        }
    }

    /// Vanilla `LivingEntity.setSpeed` — stores speed and sets forward input (`zza`).
    pub fn set_speed(&self, speed: f64) {
        self.living_speed.store(speed);
        // Match vanilla: setSpeed also sets forward speed to the same value.
        let mut input = self.movement_input.load();
        input.z = speed;
        self.movement_input.store(input);
    }

    /// Clear AI-driven speed (idle).
    pub fn clear_speed(&self) {
        self.living_speed.store(0.0);
        self.movement_input.store(Vector3::default());
    }
}

/// Returns `true` if `damage_type` is in `#minecraft:bypasses_armor` (1.21.11).
/// These sources bypass armor entirely (fall, drown, freeze, etc.).
pub(crate) const fn bypasses_armor_durability(damage_type: &DamageType) -> bool {
    // Bitmask lookup: O(1) with two instructions (shift + AND), no array scan.
    // DamageType IDs can exceed 31; use u64 for sufficient range.
    // TODO: Make data-driven once the data pack system can handle it without performance regressions.
    // Compile-time assertions: ensure all bypassing types fit in u64 bitmask.
    const _: () = assert!(
        DamageType::FALL.id < 64
            && DamageType::FLY_INTO_WALL.id < 64
            && DamageType::ON_FIRE.id < 64
            && DamageType::IN_WALL.id < 64
            && DamageType::CRAMMING.id < 64
            && DamageType::DROWN.id < 64
            && DamageType::GENERIC.id < 64
            && DamageType::WITHER.id < 64
            && DamageType::DRAGON_BREATH.id < 64
            && DamageType::STARVE.id < 64
            && DamageType::ENDER_PEARL.id < 64
            && DamageType::FREEZE.id < 64
            && DamageType::STALAGMITE.id < 64
            && DamageType::MAGIC.id < 64
            && DamageType::INDIRECT_MAGIC.id < 64
            && DamageType::OUT_OF_WORLD.id < 64
            && DamageType::GENERIC_KILL.id < 64
            && DamageType::SONIC_BOOM.id < 64
            && DamageType::OUTSIDE_BORDER.id < 64,
        "One or more bypass DamageType IDs exceed u64 bitmask width (>= 64)"
    );
    const BYPASS_MASK: u64 = (1u64 << DamageType::FALL.id)
        | (1u64 << DamageType::FLY_INTO_WALL.id)
        | (1u64 << DamageType::ON_FIRE.id)
        | (1u64 << DamageType::IN_WALL.id)
        | (1u64 << DamageType::CRAMMING.id)
        | (1u64 << DamageType::DROWN.id)
        | (1u64 << DamageType::GENERIC.id)
        | (1u64 << DamageType::WITHER.id)
        | (1u64 << DamageType::DRAGON_BREATH.id)
        | (1u64 << DamageType::STARVE.id)
        | (1u64 << DamageType::ENDER_PEARL.id)
        | (1u64 << DamageType::FREEZE.id)
        | (1u64 << DamageType::STALAGMITE.id)
        | (1u64 << DamageType::MAGIC.id)
        | (1u64 << DamageType::INDIRECT_MAGIC.id)
        | (1u64 << DamageType::OUT_OF_WORLD.id)
        | (1u64 << DamageType::GENERIC_KILL.id)
        | (1u64 << DamageType::SONIC_BOOM.id)
        | (1u64 << DamageType::OUTSIDE_BORDER.id);
    (damage_type.id < 64) && ((BYPASS_MASK >> damage_type.id) & 1 == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pumpkin_util::version::JavaMinecraftVersion;

    fn encoded_living_flags(version: JavaMinecraftVersion, flags: u8) -> Vec<u8> {
        let mut encoded = Vec::new();
        for metadata in LivingEntity::living_flags_metadata(flags) {
            metadata.write(&mut encoded, &version).unwrap();
        }
        encoded
    }

    #[test]
    fn active_hand_flags_follow_vanilla_main_off_hand_and_stop_states() {
        assert_eq!(
            LivingEntity::with_active_hand_flags(0, Some(Hand::Right)),
            LivingEntity::USING_ITEM_FLAG
        );
        assert_eq!(
            LivingEntity::with_active_hand_flags(0, Some(Hand::Left)),
            LivingEntity::USING_ITEM_FLAG | LivingEntity::OFF_HAND_ACTIVE_FLAG
        );
        assert_eq!(
            LivingEntity::with_active_hand_flags(
                LivingEntity::ACTIVE_HAND_FLAGS | LivingEntity::USING_RIPTIDE_FLAG,
                None,
            ),
            LivingEntity::USING_RIPTIDE_FLAG
        );
    }

    #[test]
    fn using_item_metadata_uses_the_1_21_living_flags_accessor() {
        for version in [
            JavaMinecraftVersion::V_1_21,
            JavaMinecraftVersion::V_1_21_2,
            JavaMinecraftVersion::V_1_21_4,
            JavaMinecraftVersion::V_1_21_5,
            JavaMinecraftVersion::V_1_21_6,
            JavaMinecraftVersion::V_1_21_7,
            JavaMinecraftVersion::V_1_21_9,
            JavaMinecraftVersion::V_1_21_11,
        ] {
            assert_eq!(
                encoded_living_flags(version, LivingEntity::USING_ITEM_FLAG),
                vec![8, 0, LivingEntity::USING_ITEM_FLAG],
                "{version:?} should receive the living flags metadata"
            );
        }
    }

    #[test]
    fn using_item_metadata_uses_the_26_living_entity_flags_accessor() {
        for version in [JavaMinecraftVersion::V_26_1, JavaMinecraftVersion::V_26_2] {
            assert_eq!(
                encoded_living_flags(version, LivingEntity::USING_ITEM_FLAG),
                vec![8, 0, LivingEntity::USING_ITEM_FLAG],
                "{version:?} should receive the living entity flags metadata"
            );
        }
    }

    #[test]
    fn using_item_metadata_serializes_the_release_state() {
        for version in [
            JavaMinecraftVersion::V_1_21,
            JavaMinecraftVersion::V_1_21_11,
            JavaMinecraftVersion::V_26_1,
            JavaMinecraftVersion::V_26_2,
        ] {
            assert_eq!(
                encoded_living_flags(version, 0),
                vec![8, 0, 0],
                "{version:?} should receive a clear using-item flag"
            );
        }
    }

    // ── bypasses_armor_durability ─────────────────────────────────────

    /// Every member of `minecraft:bypasses_armor` (1.21.11) must return `true`.
    #[test]
    fn bypasses_armor_durability_returns_true_for_tag_members() {
        // Exact contents of the minecraft:bypasses_armor tag in 1.21.11.
        let bypassing: &[DamageType] = &[
            DamageType::ON_FIRE,
            DamageType::IN_WALL,
            DamageType::CRAMMING,
            DamageType::DROWN,
            DamageType::FLY_INTO_WALL,
            DamageType::GENERIC,
            DamageType::WITHER,
            DamageType::DRAGON_BREATH,
            DamageType::STARVE,
            DamageType::FALL,
            DamageType::ENDER_PEARL,
            DamageType::FREEZE,
            DamageType::STALAGMITE,
            DamageType::MAGIC,
            DamageType::INDIRECT_MAGIC,
            DamageType::OUT_OF_WORLD,
            DamageType::GENERIC_KILL,
            DamageType::SONIC_BOOM,
            DamageType::OUTSIDE_BORDER,
        ];
        for dt in bypassing {
            assert!(
                bypasses_armor_durability(dt),
                "{dt:?} should bypass armor durability"
            );
        }
    }

    /// Physical/combat damage types must NOT bypass armor durability.
    #[test]
    fn bypasses_armor_durability_returns_false_for_physical_sources() {
        let physical: &[DamageType] = &[
            DamageType::MOB_ATTACK,
            DamageType::PLAYER_ATTACK,
            DamageType::ARROW,
            DamageType::CACTUS,
            DamageType::SWEET_BERRY_BUSH,
            DamageType::LAVA,
            DamageType::EXPLOSION,
            DamageType::PLAYER_EXPLOSION,
            DamageType::LIGHTNING_BOLT,
            DamageType::FIREBALL,
            DamageType::THORNS,
            DamageType::TRIDENT,
        ];
        for dt in physical {
            assert!(
                !bypasses_armor_durability(dt),
                "{dt:?} should NOT bypass armor durability"
            );
        }
    }

    #[test]
    fn hurt_sound_for_entity_uses_zombie_family_sounds() {
        let cases = [
            (&EntityType::ZOMBIE, Sound::EntityZombieHurt),
            (&EntityType::DROWNED, Sound::EntityDrownedHurt),
            (&EntityType::HUSK, Sound::EntityHuskHurt),
            (
                &EntityType::ZOMBIE_VILLAGER,
                Sound::EntityZombieVillagerHurt,
            ),
        ];

        for (entity_type, expected) in cases {
            assert_eq!(LivingEntity::hurt_sound_for_entity(entity_type), expected);
        }
    }

    #[test]
    fn hurt_sound_for_entity_uses_enderman_hurt_sound() {
        assert_eq!(
            LivingEntity::hurt_sound_for_entity(&EntityType::ENDERMAN),
            Sound::EntityEndermanHurt
        );
    }

    #[test]
    fn hurt_sound_for_entity_uses_skeleton_family_sounds() {
        let cases = [
            (&EntityType::SKELETON, Sound::EntitySkeletonHurt),
            (&EntityType::BOGGED, Sound::EntityBoggedHurt),
            (&EntityType::PARCHED, Sound::EntityParchedHurt),
            (
                &EntityType::WITHER_SKELETON,
                Sound::EntityWitherSkeletonHurt,
            ),
            (&EntityType::STRAY, Sound::EntityStrayHurt),
        ];

        for (entity_type, expected) in cases {
            assert_eq!(LivingEntity::hurt_sound_for_entity(entity_type), expected);
        }
    }

    #[test]
    fn hurt_sound_for_entity_resolves_registry_name() {
        // entities.json omits most hurt_sound fields; we resolve entity.<name>.hurt.
        assert_eq!(
            LivingEntity::hurt_sound_for_entity(&EntityType::CREEPER),
            Sound::EntityCreeperHurt
        );
        assert_eq!(
            LivingEntity::hurt_sound_for_entity(&EntityType::VILLAGER),
            Sound::EntityVillagerHurt
        );
    }
}

// `living.rs` 拆分为 `living/` 子模块后的编译期回归网。每条绑定把一个迁出的
// 方法钉在它原本的签名上；这里编译失败就意味着代码搬运改动或丢失了 API。
// 与 `entity/mod.rs::split_reachability` 同一思路，放在本模块以免跨文件改动。
#[cfg(test)]
mod split_reachability {
    use super::LivingEntity;
    use crate::entity::Entity;
    use pumpkin_data::attributes::Attributes;
    use pumpkin_data::data_component_impl::EquipmentSlot;
    use pumpkin_data::item_stack::ItemStack;
    use pumpkin_protocol::java::client::play::CSetEquipment;
    use pumpkin_util::math::vector3::Vector3;

    // async 方法无法写成普通 `fn` 指针，改为按值传入；方法消失时同样解析失败。
    const fn probe<F: Copy>(_: F) {}

    // equipment.rs
    const _: fn(&LivingEntity) -> Option<CSetEquipment> = LivingEntity::equipment_packet_if_any;
    const _: fn(&LivingEntity, &Entity, u32) = LivingEntity::pickup;
    const _: fn(&LivingEntity, &[(EquipmentSlot, ItemStack)]) =
        LivingEntity::send_equipment_changes;
    const _: () = probe(LivingEntity::set_active_hand);
    const _: () = probe(LivingEntity::clear_active_hand);
    const _: () = probe(LivingEntity::is_blocking);
    const _: () = probe(LivingEntity::swing_hand);
    const _: () = probe(LivingEntity::held_item);
    const _: () = probe(LivingEntity::get_stack_in_hand);
    const _: () = probe(LivingEntity::off_hand_item);

    // effects.rs
    const _: fn(&LivingEntity, f32) = LivingEntity::heal;
    const _: fn(&LivingEntity, f32) = LivingEntity::set_health;
    const _: fn(&LivingEntity) -> f32 = LivingEntity::get_max_health;
    const _: fn(&LivingEntity) -> f32 = LivingEntity::get_absorption;
    const _: fn(&LivingEntity, &Attributes) -> f64 = LivingEntity::get_attribute_value;
    const _: fn(&LivingEntity, &Attributes) -> f64 = LivingEntity::get_attribute_base;
    const _: fn(&LivingEntity, &Attributes, f64) = LivingEntity::set_attribute_base;
    const _: () = probe(LivingEntity::set_max_health);
    const _: () = probe(LivingEntity::set_absorption);
    const _: () = probe(LivingEntity::add_effect);
    const _: () = probe(LivingEntity::remove_effect);
    const _: () = probe(LivingEntity::has_effect);
    const _: () = probe(LivingEntity::get_effect);
    const _: () = probe(LivingEntity::reset_effects_and_attributes);

    // movement.rs
    const _: fn(&LivingEntity) -> bool = LivingEntity::is_in_water;
    const _: fn(&LivingEntity) -> f64 = LivingEntity::get_swim_height;
    const _: fn(&LivingEntity) -> bool = LivingEntity::is_in_powder_snow;
    const _: fn(&LivingEntity) -> bool = LivingEntity::should_prevent_fall_damage;
    const _: fn(&LivingEntity) -> bool = LivingEntity::should_prevent_fall_damage_in_area;
    const _: fn(&LivingEntity) -> bool = LivingEntity::is_immune_to_fall_damage;
    const _: () = probe(LivingEntity::is_in_fall_damage_resetting);
    const _: () = probe(LivingEntity::fall);
    const _: () = probe(LivingEntity::handle_fall_damage);

    // death.rs
    const _: () = probe(LivingEntity::on_death);
    const _: () = probe(LivingEntity::get_death_message);

    // entity_base.rs（inherent 部分）
    const _: fn(&LivingEntity) -> i32 = LivingEntity::entity_id;
    const _: fn(&LivingEntity) -> bool = LivingEntity::is_alive;
    const _: fn(&LivingEntity) -> bool = LivingEntity::is_part_of_game;
    const _: fn(&LivingEntity) -> bool = LivingEntity::can_take_damage;
    const _: fn(&LivingEntity) -> bool = LivingEntity::is_player;
    const _: fn(&LivingEntity) -> Vector3<f64> = LivingEntity::get_movement;
    const _: () = probe(LivingEntity::reset_state);
}
