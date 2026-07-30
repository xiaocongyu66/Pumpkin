//! Iron golem AI synced to **Minecraft 26.2** vanilla NMS
//! (`net.minecraft.world.entity.animal.golem.IronGolem`).
//!
//! Decompiled from official `server-26.2.jar` (protocol 776 / Pumpkin CURRENT).
//! Paper/Leaves leave `registerGoals` + `doHurtTarget` knockback unpatched
//! (only Bukkit target-reason / spawn-in-air options).

use std::sync::{Arc, Weak};

use pumpkin_data::{
    entity::EntityType,
    item::Item,
    item_stack::ItemStack,
    sound::{Sound, SoundCategory},
};
use pumpkin_nbt::compound::NbtCompound;

use crate::entity::{
    Entity, EntityBaseFuture, NBTStorage, NbtFuture,
    ai::{
        goal::{
            active_target::ActiveTargetGoal, defend_villagers::DefendVillagersGoal,
            look_around::RandomLookAroundGoal, look_at_entity::LookAtEntityGoal,
            melee_attack::MeleeAttackGoal, move_towards_target::MoveTowardsTargetGoal,
            revenge::RevengeGoal, wander_around::WanderAroundGoal,
        },
        pathfinder::node::PathType,
        vanilla_enemy::{IRON_GOLEM_ENEMY_EXCLUDES, IRON_GOLEM_TARGET_CHANCE},
    },
    mob::{Mob, MobEntity},
    player::Player,
};

/// 铁锭修复铁傀儡的治疗量（原版 `IRON_INGOT_HEAL_AMOUNT`）。
const IRON_INGOT_HEAL_AMOUNT: f32 = 25.0;

/// Iron golem — Vanilla 26.2 `IronGolem`.
///
/// # `registerGoals()` (26.2)
/// ```text
/// goalSelector:
///   1 MeleeAttackGoal(this, 1.0, true)
///   2 MoveTowardsTargetGoal(this, 0.9, 32.0f)
///   2 MoveBackToVillageGoal(this, 0.6, false)     // TODO village POI
///   4 GolemRandomStrollInVillageGoal(this, 0.6)   // ≈ WanderAround 0.6
///   5 OfferFlowerGoal(this)                       // TODO
///   7 LookAtPlayerGoal(this, Player, 6.0f)
///   8 RandomLookAroundGoal(this)
/// targetSelector:
///   1 DefendVillageTargetGoal(this)               // TODO villager reputation
///   2 HurtByTargetGoal(this)
///   3 NearestAttackableTargetGoal(Player, 10, true, false, isAngryAt) // NeutralMob
///   3 NearestAttackableTargetGoal(Mob, 5, false, false,
///         e -> e instanceof Enemy && !(e instanceof Creeper))
///   4 ResetUniversalAngerTargetGoal(this, false)  // TODO NeutralMob timer
/// ```
///
/// # `doHurtTarget` (26.2) — see `MobEntity::try_attack`
/// ```text
/// attackAnimationTick = 10;
/// broadcastEntityEvent(byte 4);
/// damage = f/2 + random(0..floor(f));
/// if (hurt) {
///   scale = max(0, 1 - knockbackResistance);
///   deltaMovement += (0, 0.4 * scale, 0);  // vertical only
/// }
/// playSound(IRON_GOLEM_ATTACK);
/// ```
///
/// # `canAttack` (26.2)
/// Never creeper; player-created golems never attack players.
///
/// Wiki: <https://minecraft.wiki/w/Iron_Golem>
pub struct IronGolemEntity {
    pub mob_entity: MobEntity,
}

impl IronGolemEntity {
    pub fn new(entity: Entity) -> Arc<Self> {
        let mob_entity = MobEntity::new(entity);

        // Prefer dry land (water malus < 0 → impassable for pathfinder).
        // Vanilla size 1.4×2.7 + STEP_HEIGHT 1.0 (pathfinder used zombie defaults before).
        {
            let mut nav = mob_entity.navigator.lock().unwrap();
            nav.set_pathfinding_malus(PathType::Water, -1.0);
            nav.set_pathfinding_malus(PathType::WaterBorder, -1.0);
            let dim = EntityType::IRON_GOLEM.dimension;
            nav.set_mob_dimensions(dim[0], dim[1]);
        }

        let iron_golem = Self { mob_entity };
        let mob_arc = Arc::new(iron_golem);
        let mob_weak: Weak<dyn Mob> = {
            let mob_arc: Arc<dyn Mob> = mob_arc.clone();
            Arc::downgrade(&mob_arc)
        };

        {
            let mut goal_selector = mob_arc.mob_entity.goals_selector.lock().unwrap();
            let mut target_selector = mob_arc.mob_entity.target_selector.lock().unwrap();

            // --- goalSelector (priority order from 26.2) ---
            // No FloatGoal: iron golems do not swim (decreaseAirSupply is a no-op so they
            // never drown, but they walk/sink rather than float).
            // 1 MeleeAttackGoal(this, 1.0, true)  — followingTargetEvenIfNotSeen
            goal_selector.add_goal(1, Box::new(MeleeAttackGoal::new(1.0, true)));
            // 2 MoveTowardsTargetGoal(this, 0.9, 32.0f)
            goal_selector.add_goal(2, Box::new(MoveTowardsTargetGoal::new(0.9, 32.0)));
            // 4 GolemRandomStrollInVillageGoal(this, 0.6) ≈ general wander at 0.6
            goal_selector.add_goal(4, Box::new(WanderAroundGoal::new(0.6)));
            // 7 LookAtPlayerGoal(this, Player.class, 6.0f)
            goal_selector.add_goal(
                7,
                LookAtEntityGoal::with_default(mob_weak, &EntityType::PLAYER, 6.0),
            );
            // 8 RandomLookAroundGoal(this)
            goal_selector.add_goal(8, Box::new(RandomLookAroundGoal::default()));

            // --- targetSelector ---
            // 1 DefendVillageTargetGoal
            target_selector.add_goal(1, DefendVillagersGoal::new());
            // 2 HurtByTargetGoal(this)
            target_selector.add_goal(2, Box::new(RevengeGoal::new(true)));
            // 3 NearestAttackableTargetGoal(Mob, 5, false, false, Enemy && !Creeper)
            //    → includes Warden (Enemy/MONSTER); excludes only Creeper
            target_selector.add_goal(
                3,
                ActiveTargetGoal::for_enemies(
                    &mob_arc.mob_entity,
                    IRON_GOLEM_ENEMY_EXCLUDES,
                    IRON_GOLEM_TARGET_CHANCE,
                    false,
                ),
            );
        };

        mob_arc
    }
}

impl NBTStorage for IronGolemEntity {
    fn write_nbt<'a>(&'a self, nbt: &'a mut NbtCompound) -> NbtFuture<'a, ()> {
        self.mob_entity.living_entity.write_nbt(nbt)
    }

    fn read_nbt_non_mut<'a>(&'a self, nbt: &'a NbtCompound) -> NbtFuture<'a, ()> {
        self.mob_entity.living_entity.read_nbt_non_mut(nbt)
    }
}

impl Mob for IronGolemEntity {
    fn get_mob_entity(&self) -> &MobEntity {
        &self.mob_entity
    }

    /// 原版 `IronGolem.mobInteract`（26.2，IronGolem.java:266-280）。
    ///
    /// ```text
    /// if (!itemStack.is(Items.IRON_INGOT)) return PASS;
    /// float healthBefore = getHealth();
    /// heal(25.0f);
    /// if (getHealth() == healthBefore) return PASS;   // 满血时不消耗铁锭
    /// float pitch = 1.0f + (random.nextFloat() - random.nextFloat()) * 0.2f;
    /// playSound(IRON_GOLEM_REPAIR, 1.0f, pitch);
    /// itemStack.consume(1, player);
    /// return SUCCESS;
    /// ```
    fn mob_interact<'a>(
        &'a self,
        player: &'a Arc<Player>,
        item_stack: &'a mut ItemStack,
    ) -> EntityBaseFuture<'a, bool> {
        Box::pin(async move {
            // 非铁锭：走默认交互（拴绳等）。
            if item_stack.item != &Item::IRON_INGOT {
                return self.mob_entity.mob_interact(player, item_stack).await;
            }

            let living = &self.mob_entity.living_entity;
            let health_before = living.health.load();
            // heal() 内部断言 additional_health > 0 并夹取到最大生命值。
            living.heal(IRON_INGOT_HEAL_AMOUNT);
            if living.health.load() == health_before {
                // 已满血：不播放音效也不消耗铁锭（原版返回 PASS）。
                return false;
            }

            let entity = &living.entity;
            let pitch = {
                use rand::RngExt;
                let mut rng = rand::rng();
                1.0 + (rng.random::<f32>() - rng.random::<f32>()) * 0.2
            };
            entity.world.load().play_sound_fine(
                Sound::EntityIronGolemRepair,
                SoundCategory::Neutral,
                &entity.pos.load(),
                1.0,
                pitch,
            );

            // 原版 consume(1, player)：创造模式不扣物品。
            item_stack.decrement_unless_creative(player.gamemode.load(), 1);
            true
        })
    }
}
