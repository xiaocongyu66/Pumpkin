//! 监守者（远古城市 / 深暗之域），对应原版
//! `monster/warden/Warden.java`（670 行）。
//!
//! 原版是 Brain + Behavior + MemoryModule 体系（`WardenAi.java` 编排），
//! Pumpkin 没有 Brain，映射关系如下：
//!
//! | 原版 | 本实现 |
//! |---|---|
//! | `AngerManagement` | [`anger::AngerManagement`]（`anger.rs`） |
//! | `AngerLevel` | [`anger::AngerLevel`]（`anger.rs`） |
//! | `Activity.ROAR` / `Roar` | [`ai::RoarGoal`] |
//! | `Activity.INVESTIGATE` | [`ai::InvestigateDisturbanceGoal`] |
//! | `Activity.SNIFF` / `Sniffing` | [`ai::SniffGoal`] |
//! | `Activity.FIGHT` / `SonicBoom` | `MeleeAttackGoal` + [`SonicBoomGoal`] |
//! | `MemoryModuleType.ROAR_TARGET` | `WardenEntity::roar_target` 字段 |
//! | `MemoryModuleType.DISTURBANCE_LOCATION` | `WardenEntity::disturbance_location` 字段 |
//! | `MemoryModuleType.VIBRATION_COOLDOWN` | `WardenEntity::vibration_cooldown` 字段 |
//! | `VibrationSystem.User` | [`vibration`] 模块 |
//!
//! 未实现：`Activity.EMERGE`/`Activity.DIG`（出土与钻地）与
//! `WardenSpawnTracker`，两者都依赖 Pumpkin 尚未提供的生成钩子。

pub mod ai;
pub mod anger;
pub mod vibration;

use std::sync::Arc;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::Ordering::Relaxed;

use crossbeam::atomic::AtomicCell;

use pumpkin_data::damage::DamageType;
use pumpkin_data::effect::StatusEffect;
use pumpkin_data::entity::{EntityPose, EntityType};
use pumpkin_data::meta_data_type::MetaDataType;
use pumpkin_data::potion::Effect;
use pumpkin_data::sound::SoundCategory;
use pumpkin_data::tracked_data::TrackedData;
use pumpkin_nbt::compound::NbtCompound;
use pumpkin_nbt::tag::NbtTag;
use pumpkin_protocol::codec::var_int::VarInt;
use pumpkin_protocol::java::client::play::Metadata;
use pumpkin_util::math::position::BlockPos;
use pumpkin_util::math::vector3::Vector3;
use uuid::Uuid;

use crate::entity::ai::goal::{
    look_around::RandomLookAroundGoal, melee_attack::MeleeAttackGoal, sonic_boom::SonicBoomGoal,
    swim::SwimGoal, wander_around::WanderAroundGoal,
};
use crate::entity::mob::{Mob, MobEntity};
use crate::entity::{Entity, EntityBase, EntityBaseFuture, NBTStorage, NbtFuture};

use self::ai::{InvestigateDisturbanceGoal, RoarGoal, SniffGoal};
use self::anger::{AngerLevel, AngerManagement};

/// 原版 `Warden.VIBRATION_COOLDOWN_TICKS`（Warden.java:91）。
const VIBRATION_COOLDOWN_TICKS: i32 = 40;

/// 原版 `Warden.TIME_TO_USE_MELEE_UNTIL_SONIC_BOOM`（Warden.java:92）：
/// 刚锁定目标时先给音爆压 200 tick 冷却，让监守者先近战。
const TIME_TO_USE_MELEE_UNTIL_SONIC_BOOM: i32 = 200;

/// 原版 `Warden.FOLLOW_RANGE`（Warden.java:98）。注意是 24，不是旧实现里的 32。
const FOLLOW_RANGE: f64 = 24.0;

/// 原版 `Warden.DARKNESS_DISPLAY_LIMIT`（Warden.java:100）。
const DARKNESS_DISPLAY_LIMIT: i32 = 200;

/// 原版 `Warden.DARKNESS_DURATION`（Warden.java:101）。
const DARKNESS_DURATION: i32 = 260;

/// 原版 `Warden.DARKNESS_RADIUS`（Warden.java:102）。
const DARKNESS_RADIUS: f64 = 20.0;

/// 原版 `Warden.DARKNESS_INTERVAL`（Warden.java:103）。
const DARKNESS_INTERVAL: i32 = 120;

/// 原版 `Warden.ANGERMANAGEMENT_TICK_DELAY`（Warden.java:104）。
const ANGERMANAGEMENT_TICK_DELAY: i32 = 20;

/// 原版 `Warden.DEFAULT_ANGER`（Warden.java:105）：一次普通振动的愤怒增量。
pub const DEFAULT_ANGER: i32 = 35;

/// 原版 `Warden.PROJECTILE_ANGER`（Warden.java:106）。
const PROJECTILE_ANGER: i32 = 10;

/// 原版 `Warden.ON_HURT_ANGER_BOOST`（Warden.java:107）。
const ON_HURT_ANGER_BOOST: i32 = 20;

/// 原版 `Warden.TOUCH_COOLDOWN_TICKS`（Warden.java:109）。
const TOUCH_COOLDOWN_TICKS: i32 = 20;

/// 原版 `Warden.PROJECTILE_ANGER_DISTANCE`（Warden.java:113）。
const PROJECTILE_ANGER_DISTANCE: f64 = 30.0;

/// 原版 `WardenAi.DISTURBANCE_LOCATION_EXPIRY_TIME`（WardenAi.java:66）。
const DISTURBANCE_LOCATION_EXPIRY_TIME: i32 = 100;

/// 原版 `Warden.VibrationUser.GAME_EVENT_LISTENER_RANGE`（Warden.java:596）。
const GAME_EVENT_LISTENER_RANGE: f64 = 16.0;

/// 原版 `Mob.getAmbientSoundInterval`（Mob.java:318-320）。
const AMBIENT_SOUND_INTERVAL: i32 = 80;

/// 原版 `Warden`（Warden.java:88-668）。
pub struct WardenEntity {
    pub mob_entity: MobEntity,
    /// 原版 `Warden.angerManagement`（Warden.java:128）。
    anger_management: tokio::sync::Mutex<AngerManagement>,
    /// 原版 `MemoryModuleType.ROAR_TARGET`：等待咆哮的目标 UUID。
    roar_target: tokio::sync::Mutex<Option<Uuid>>,
    /// 原版 `MemoryModuleType.DISTURBANCE_LOCATION`：听到的声源位置。
    disturbance_location: tokio::sync::Mutex<Option<BlockPos>>,
    /// 原版 `MemoryModuleType.DISTURBANCE_LOCATION` 的 100 tick 过期计时。
    disturbance_ticks: AtomicI32,
    /// 原版 `MemoryModuleType.VIBRATION_COOLDOWN`（Warden.java:642）：
    /// 收到一次振动后 40 tick 内不再响应。
    vibration_cooldown: AtomicI32,
    /// 原版 `MemoryModuleType.TOUCH_COOLDOWN`（Warden.java:546）。
    touch_cooldown: AtomicI32,
    /// 原版同步字段 `CLIENT_ANGER_LEVEL`（Warden.java:99）的缓存，
    /// 避免每 20 tick 重复发同一个 metadata 包。
    client_anger_level: AtomicI32,
    /// 原版 `SonicBoom.setCooldown(this, 200)`（Warden.java:526）与
    /// `SonicBoom.setCooldown(this, 40)`（Warden.java:236）写入的
    /// `MemoryModuleType.SONIC_BOOM_COOLDOWN`。原版由 Brain 持有，这里放在实体上，
    /// 由 [`SonicBoomGoal`] 每次 `can_start` 时读取并递减。
    pub sonic_boom_cooldown: AtomicI32,
    /// 世界边界快照（中心 X / 中心 Z / 直径），每 tick 由
    /// [`Self::refresh_border_snapshot`] 刷新，供同步的 [`Self::is_within_border`] 读取。
    /// 存快照而非每次取锁，是为了让 `can_target_entity` 等同步谓词无需 async 化。
    border_center_x: AtomicCell<f64>,
    border_center_z: AtomicCell<f64>,
    border_diameter: AtomicCell<f64>,
    /// 原版 `Mob.ambientSoundTime`：环境音计时，见 [`Self::tick_ambient_sound`]。
    ambient_sound_time: AtomicI32,
}

impl WardenEntity {
    pub fn new(entity: Entity) -> Arc<Self> {
        let mob_entity = MobEntity::new(entity);
        // 原版 `Warden.createAttributes`（Warden.java:195-197）。EntityType 表里
        // 已带 MAX_HEALTH 500 / MOVEMENT_SPEED 0.3 / KNOCKBACK_RESISTANCE 1.0 /
        // ATTACK_KNOCKBACK 1.5 / ATTACK_DAMAGE 30 / FOLLOW_RANGE 24，
        // 这里只把 FOLLOW_RANGE 显式对齐原版的 24（旧实现误设为 32）。
        mob_entity.living_entity.set_attribute_base(
            &pumpkin_data::attributes::Attributes::FOLLOW_RANGE,
            FOLLOW_RANGE,
        );

        let warden = Self {
            mob_entity,
            anger_management: tokio::sync::Mutex::new(AngerManagement::default()),
            roar_target: tokio::sync::Mutex::new(None),
            disturbance_location: tokio::sync::Mutex::new(None),
            disturbance_ticks: AtomicI32::new(0),
            vibration_cooldown: AtomicI32::new(0),
            touch_cooldown: AtomicI32::new(0),
            client_anger_level: AtomicI32::new(-1),
            sonic_boom_cooldown: AtomicI32::new(0),
            // 初值对齐 `World::new` 里 `Worldborder::new(0.0, 0.0, 5.999_996_8E7, ..)`，
            // 避免第一次 `refresh_border_snapshot` 之前边界判定把所有目标都判成越界。
            border_center_x: AtomicCell::new(0.0),
            border_center_z: AtomicCell::new(0.0),
            border_diameter: AtomicCell::new(5.999_996_8E7),
            ambient_sound_time: AtomicI32::new(0),
        };
        let mob_arc = Arc::new(warden);
        let warden_weak = Arc::downgrade(&mob_arc);

        {
            let mut goal_selector = mob_arc.mob_entity.goals_selector.lock().unwrap();

            // 原版 `WardenAi.initCoreActivity` 的 `Swim(0.8f)`（WardenAi.java:83）。
            goal_selector.add_goal(0, Box::new(SwimGoal::default()));
            // 活动优先级照抄 `WardenAi.updateActivity`（WardenAi.java:79）：
            // ROAR > FIGHT > INVESTIGATE > SNIFF > IDLE。
            goal_selector.add_goal(1, RoarGoal::new(warden_weak.clone()));
            // 原版 FIGHT 活动内 SonicBoom 排在 MeleeAttack 之前（WardenAi.java:111）。
            goal_selector.add_goal(2, SonicBoomGoal::new(warden_weak.clone()));
            // 原版 `MeleeAttack.create(18)` + 追击速度 1.2（WardenAi.java:111、59）。
            goal_selector.add_goal(3, Box::new(MeleeAttackGoal::new(1.2, false)));
            goal_selector.add_goal(4, InvestigateDisturbanceGoal::new(warden_weak.clone()));
            goal_selector.add_goal(5, SniffGoal::new(warden_weak));
            // 原版 IDLE 活动的 `RandomStroll.stroll(0.5f)`（WardenAi.java:95）。
            goal_selector.add_goal(6, Box::new(WanderAroundGoal::new(0.5)));
            goal_selector.add_goal(7, Box::new(RandomLookAroundGoal::default()));
        };

        // 原版监守者不靠 target_selector 找目标：它只通过愤怒管理锁定目标
        // （`Warden.getTarget` 走 Brain 的 ATTACK_TARGET，Warden.java:485-488），
        // 所以这里不注册 ActiveTargetGoal / RevengeGoal，
        // 攻击目标全部由 [`Self::update_anger`] 与 [`Self::on_damage`] 驱动。

        mob_arc
    }

    /// 原版 `getWorldBorder().isWithinBounds(...)`。
    ///
    /// `World::worldborder` 是 `tokio::sync::Mutex`，取它必须 `.await`；而本判定被
    /// `can_target_entity` 等一批同步谓词（含 `is_some_and` 闭包）调用，若整条链
    /// async 化会污染 8 个调用点且闭包无法直接 await。世界边界变化极缓慢（仅
    /// `/worldborder` 指令与插值推进），所以改为每 tick 由 [`Self::refresh_border_snapshot`]
    /// 刷新 `contains` 所需的三个标量，这里只读快照做同步判定。
    fn is_within_border(&self, pos: Vector3<f64>) -> bool {
        let center_x = self.border_center_x.load();
        let center_z = self.border_center_z.load();
        let half = self.border_diameter.load() / 2.0;
        // 与 `Worldborder::contains`（border.rs:104-111）保持一致的半开区间。
        pos.x >= center_x - half
            && pos.x < center_x + half
            && pos.z >= center_z - half
            && pos.z < center_z + half
    }

    /// 每 tick 刷新一次世界边界快照，供同步的 [`Self::is_within_border`] 使用。
    async fn refresh_border_snapshot(&self) {
        let world = self.mob_entity.living_entity.entity.world.load();
        let (center_x, center_z, diameter) = {
            let border = world.worldborder.lock().await;
            (border.center_x, border.center_z, border.new_diameter)
        };
        self.border_center_x.store(center_x);
        self.border_center_z.store(center_z);
        self.border_diameter.store(diameter);
    }

    /// 原版 `Warden.canTargetEntity`（Warden.java:406-419）。
    #[must_use]
    pub fn can_target_entity(&self, entity: &dyn EntityBase) -> bool {
        // 原版首条：必须是 LivingEntity。
        let Some(living) = entity.get_living_entity() else {
            return false;
        };
        let target_entity = entity.get_entity();
        let self_entity = &self.mob_entity.living_entity.entity;
        // 原版 `this.level() != entity.level()`。
        if !Arc::ptr_eq(&self_entity.world.load(), &target_entity.world.load()) {
            return false;
        }
        // 原版 `EntitySelector.NO_CREATIVE_OR_SPECTATOR`。
        if entity.is_spectator() || entity.get_player().is_some_and(|p| p.is_creative()) {
            return false;
        }
        // 原版排除自身、盔甲架与其他监守者。
        if target_entity.entity_id == self_entity.entity_id
            || target_entity.entity_type.id == EntityType::ARMOR_STAND.id
            || target_entity.entity_type.id == EntityType::WARDEN.id
        {
            return false;
        }
        // 原版 `isInvulnerable()` 与 `isDeadOrDying()`。
        if target_entity.invulnerable.load(Relaxed) || !living.is_alive() {
            return false;
        }
        // 原版 `getWorldBorder().isWithinBounds(boundingBox)`。
        self.is_within_border(target_entity.pos.load())
    }

    /// 按 UUID 做 [`Self::can_target_entity`] 判定，供愤怒表的衰减与排序回调使用。
    fn can_target_uuid(&self, uuid: Uuid) -> bool {
        let world = self.mob_entity.living_entity.entity.world.load();
        world
            .get_entity_by_uuid(uuid)
            .is_some_and(|entity| self.can_target_entity(entity.as_ref()))
    }

    /// 原版 `Warden.getAngerLevel`（Warden.java:447-449）。
    pub async fn anger_level(&self) -> AngerLevel {
        AngerLevel::by_anger(self.active_anger().await)
    }

    /// 原版 `Warden.getActiveAnger`（Warden.java:451-453）：
    /// 有攻击目标就看该目标的愤怒值，否则取全表最高。
    async fn active_anger(&self) -> i32 {
        let target_uuid = self
            .mob_entity
            .target
            .lock()
            .await
            .as_ref()
            .map(|t| t.get_entity().entity_uuid);
        self.anger_management.lock().await.active_anger(target_uuid)
    }

    /// 原版 `Warden.clearAnger`（Warden.java:455-457）。
    pub async fn clear_anger(&self, uuid: Uuid) {
        self.anger_management.lock().await.clear_anger(uuid);
    }

    /// 原版 `Warden.increaseAngerAt(entity, amount, playSound)`（Warden.java:463-476）。
    ///
    /// 这是「听到声音 → 累积愤怒」链路的核心：累加后若目标是玩家且已进入 ANGRY
    /// 等级，原版会清掉当前 ATTACK_TARGET 让 `SetRoarTarget` 重新挑选
    /// （Warden.java:469-471），从而切向更值得追的玩家。
    pub async fn increase_anger_at_uuid(&self, uuid: Uuid, amount: i32, play_sound: bool) {
        if self.mob_entity.is_no_ai() {
            return;
        }
        let world = self.mob_entity.living_entity.entity.world.load();
        let Some(target) = world.get_entity_by_uuid(uuid) else {
            return;
        };
        if !self.can_target_entity(target.as_ref()) {
            return;
        }
        let is_player = target.get_player().is_some();

        let current_target_is_player = self
            .mob_entity
            .target
            .lock()
            .await
            .as_ref()
            .is_some_and(|t| t.get_player().is_some());
        let maybe_switch_target = !current_target_is_player;

        let new_anger = self
            .anger_management
            .lock()
            .await
            .increase_anger(uuid, is_player, amount);

        if is_player && maybe_switch_target && AngerLevel::by_anger(new_anger).is_angry() {
            // 原版 `eraseMemory(ATTACK_TARGET)`：让咆哮流程重新选目标。
            *self.mob_entity.target.lock().await = None;
        }
        if play_sound {
            self.play_listening_sound().await;
        }
    }

    /// 原版 `Warden.playListeningSound`（Warden.java:441-445）：
    /// 咆哮姿态下不播，音量 10.0，音效随愤怒等级变化。
    async fn play_listening_sound(&self) {
        let entity = &self.mob_entity.living_entity.entity;
        if entity.pose.load() == EntityPose::Roaring {
            return;
        }
        let sound = self.anger_level().await.listening_sound();
        entity.world.load().play_sound_fine(
            sound,
            SoundCategory::Hostile,
            &entity.pos.load(),
            10.0,
            1.0,
        );
    }

    /// 原版 `Warden.setAttackTarget`（Warden.java:522-527）：
    /// 清咆哮目标、设攻击目标，并给音爆压 200 tick 冷却让近战先打。
    pub async fn set_attack_target(&self, target: Arc<dyn EntityBase>) {
        if !self.can_target_entity(target.as_ref()) {
            return;
        }
        *self.roar_target.lock().await = None;
        *self.mob_entity.target.lock().await = Some(target);
        *self.disturbance_location.lock().await = None;
        // 原版 `SonicBoom.setCooldown(this, 200)`（Warden.java:526）：
        // 刚锁定目标先近战 200 tick 再考虑音爆。
        self.sonic_boom_cooldown
            .store(TIME_TO_USE_MELEE_UNTIL_SONIC_BOOM, Relaxed);
    }

    /// 按 UUID 查实体后走 [`Self::set_attack_target`]，供只持有 UUID 的
    /// 咆哮流程使用。
    pub async fn set_attack_target_by_uuid(&self, uuid: Uuid) {
        let world = self.mob_entity.living_entity.entity.world.load();
        if let Some(target) = world.get_entity_by_uuid(uuid) {
            self.set_attack_target(target).await;
        }
    }

    /// 原版 `WardenAi.setDisturbanceLocation`（WardenAi.java:131-140）：
    /// 已经愤怒或已有攻击目标时不写骚动位置（那时该走 ROAR/FIGHT）。
    pub async fn set_disturbance_location(&self, pos: BlockPos) {
        let entity = &self.mob_entity.living_entity.entity;
        let world = entity.world.load();
        let center = pos.to_centered_f64();
        let within_border = {
            let border = world.worldborder.lock().await;
            border.contains(center.x, center.z)
        };
        if !within_border {
            return;
        }
        if self.mob_entity.target.lock().await.is_some() || self.roar_target.lock().await.is_some()
        {
            return;
        }
        *self.disturbance_location.lock().await = Some(pos);
        self.disturbance_ticks
            .store(DISTURBANCE_LOCATION_EXPIRY_TIME, Relaxed);
    }

    /// 原版 `Warden.applyDarknessAround`（Warden.java:421-424）：
    /// 20 格内玩家获得 260 tick 黑暗，且只在剩余时间低于 200 tick 时刷新。
    async fn apply_darkness_around(&self) {
        let entity = &self.mob_entity.living_entity.entity;
        let world = entity.world.load();
        let pos = entity.pos.load();
        for player in world.get_nearby_players(pos, DARKNESS_RADIUS) {
            if player.is_spectator() || player.is_creative() {
                continue;
            }
            // 原版 `MobEffectUtil.addEffectToPlayersAround(..., 200)` 的
            // `notifyThreshold`：已有效果且剩余时间还长就不覆盖。
            let already_long = player
                .living_entity
                .get_effect(&StatusEffect::DARKNESS)
                .await
                .is_some_and(|effect| effect.duration > DARKNESS_DISPLAY_LIMIT);
            if already_long {
                continue;
            }
            player
                .add_effect(Effect {
                    effect_type: &StatusEffect::DARKNESS,
                    duration: DARKNESS_DURATION,
                    amplifier: 0,
                    ambient: false,
                    show_particles: false,
                    show_icon: false,
                    blend: false,
                })
                .await;
        }
    }

    /// 原版 `Mob.baseTick` 的环境音部分（Mob.java:330-334）：
    /// `random.nextInt(1000) < ambientSoundTime++` 时播一次，随后把计时重置为
    /// `-getAmbientSoundInterval()`（= -80，Mob.java:318-320、344-346）。
    ///
    /// 监守者的环境音由愤怒等级决定（原版 `Warden.getAmbientSound`，
    /// Warden.java:210-215）：CALM 用 `WARDEN_AMBIENT`，AGITATED 用
    /// `WARDEN_AGITATED`，ANGRY 用 `WARDEN_ANGRY`；咆哮或出土/钻地时不播。
    async fn tick_ambient_sound(&self) {
        let entity = &self.mob_entity.living_entity.entity;
        if matches!(
            entity.pose.load(),
            EntityPose::Roaring | EntityPose::Digging | EntityPose::Emerging
        ) {
            return;
        }
        let current = self.ambient_sound_time.fetch_add(1, Relaxed);
        if rand::random_range(0..1000) >= current {
            return;
        }
        self.ambient_sound_time
            .store(-AMBIENT_SOUND_INTERVAL, Relaxed);
        let sound = self.anger_level().await.ambient_sound();
        // 原版 `Warden.getSoundVolume` 为 4.0（Warden.java:205-207）。
        entity.world.load().play_sound_fine(
            sound,
            SoundCategory::Hostile,
            &entity.pos.load(),
            4.0,
            1.0,
        );
    }

    /// 原版 `Warden.syncClientAngerLevel`（Warden.java:250-252）：
    /// 把当前愤怒值同步给客户端，驱动触须抖动与心跳频率。
    async fn sync_client_anger_level(&self) {
        let anger = self.active_anger().await;
        if self.client_anger_level.swap(anger, Relaxed) == anger {
            return;
        }
        self.mob_entity.living_entity.entity.send_meta_data(
            &[Metadata::new(
                TrackedData::CLIENT_ANGER_LEVEL,
                MetaDataType::INTEGER,
                VarInt(anger),
            )],
            None,
        );
    }
}

/// 每 tick 的状态推进，对应原版 `Warden.customServerAiStep`（Warden.java:292-307）。
impl WardenEntity {
    /// 递减各计时器，对应原版那些带过期时间的 MemoryModule。
    async fn tick_cooldowns(&self) {
        for counter in [
            &self.vibration_cooldown,
            &self.touch_cooldown,
            &self.sonic_boom_cooldown,
        ] {
            let current = counter.load(Relaxed);
            if current > 0 {
                counter.store(current - 1, Relaxed);
            }
        }
        // 骚动位置到期后清除（原版 `setMemoryWithExpiry(..., 100L)`）。
        let remaining = self.disturbance_ticks.load(Relaxed);
        if remaining > 0 {
            self.disturbance_ticks.store(remaining - 1, Relaxed);
            if remaining == 1 {
                *self.disturbance_location.lock().await = None;
            }
        }
    }

    /// 原版 `SetRoarTarget.create(Warden::getEntityAngryAt)`（WardenAi.java:95、99、103）
    /// 加上 `Warden.getEntityAngryAt`（Warden.java:478-483）：
    /// 愤怒到 ANGRY 等级后，把愤怒值最高的嫌疑对象升级为咆哮目标。
    async fn update_anger(&self) {
        let is_valid = |uuid: Uuid| self.can_target_uuid(uuid);
        // 原版 `angerManagement.tick(level, this::canTargetEntity)`（Warden.java:303）。
        self.anger_management.lock().await.tick(&is_valid);

        // 原版 `StopAttackingIfTargetInvalid`（WardenAi.java:111）：目标不再愤怒或
        // 已不可攻击就放弃，并对失效目标清愤怒（WardenAi.java:118-123）。
        let current_target = self.mob_entity.target.lock().await.clone();
        if let Some(target) = current_target {
            let target_uuid = target.get_entity().entity_uuid;
            let targetable = self.can_target_entity(target.as_ref());
            if !targetable {
                self.clear_anger(target_uuid).await;
            }
            if !targetable || !self.anger_level().await.is_angry() {
                *self.mob_entity.target.lock().await = None;
            } else {
                // 目标仍然有效，无需另挑咆哮目标。
                return;
            }
        }

        // 原版 `SetRoarTarget` 要求 ROAR_TARGET 与 ATTACK_TARGET 都缺失。
        if self.roar_target.lock().await.is_some() {
            return;
        }
        if !self.anger_level().await.is_angry() {
            return;
        }
        let top_suspect = {
            let anger = self.anger_management.lock().await;
            anger.top_suspect(&is_valid)
        };
        if let Some(uuid) = top_suspect {
            *self.roar_target.lock().await = Some(uuid);
        }
    }
}

impl NBTStorage for WardenEntity {
    fn write_nbt<'a>(&'a self, nbt: &'a mut NbtCompound) -> NbtFuture<'a, ()> {
        Box::pin(async move {
            self.mob_entity.living_entity.write_nbt(nbt).await;
            // 原版 `Warden.addAdditionalSaveData` 的 `anger` 字段
            // （Warden.java:427-431），编解码见 `AngerManagement.codec`
            // （AngerManagement.java:67-69）：`{suspects: [{uuid, anger}, ...]}`。
            let suspects: Vec<NbtTag> = {
                let management = self.anger_management.lock().await;
                management
                    .suspect_pairs()
                    .map(|(uuid, anger)| {
                        let mut entry = NbtCompound::new();
                        let bits = uuid.as_u128();
                        entry.put(
                            "uuid",
                            NbtTag::IntArray(vec![
                                (bits >> 96) as i32,
                                ((bits >> 64) & 0xFFFF_FFFF) as i32,
                                ((bits >> 32) & 0xFFFF_FFFF) as i32,
                                (bits & 0xFFFF_FFFF) as i32,
                            ]),
                        );
                        entry.put_int("anger", anger);
                        NbtTag::Compound(entry)
                    })
                    .collect()
            };
            let mut anger_nbt = NbtCompound::new();
            anger_nbt.put("suspects", NbtTag::List(suspects));
            nbt.put("anger", NbtTag::Compound(anger_nbt));
        })
    }

    fn read_nbt_non_mut<'a>(&'a self, nbt: &'a NbtCompound) -> NbtFuture<'a, ()> {
        Box::pin(async move {
            self.mob_entity.living_entity.read_nbt_non_mut(nbt).await;
            // 原版 `Warden.readAdditionalSaveData`（Warden.java:434-439）：读不到就
            // 退回空的 AngerManagement。
            let Some(suspects) = nbt
                .get_compound("anger")
                .and_then(|anger| anger.get_list("suspects"))
            else {
                return;
            };
            let mut management = self.anger_management.lock().await;
            for tag in suspects {
                let Some(entry) = tag.extract_compound() else {
                    continue;
                };
                let Some(bits) = entry.get_int_array("uuid") else {
                    continue;
                };
                if bits.len() != 4 {
                    continue;
                }
                let uuid = Uuid::from_u128(
                    ((bits[0] as u32 as u128) << 96)
                        | ((bits[1] as u32 as u128) << 64)
                        | ((bits[2] as u32 as u128) << 32)
                        | (bits[3] as u32 as u128),
                );
                // 原版 codec 用 `ExtraCodecs.NON_NEGATIVE_INT`，负值视为损坏跳过。
                if let Some(anger) = entry.get_int("anger").filter(|anger| *anger >= 0) {
                    management.insert_saved(uuid, anger);
                }
            }
        })
    }
}

impl Mob for WardenEntity {
    fn get_mob_entity(&self) -> &MobEntity {
        &self.mob_entity
    }

    fn mob_tick<'a>(&'a self, _caller: &'a Arc<dyn EntityBase>) -> EntityBaseFuture<'a, ()> {
        Box::pin(async move {
            if !self.mob_entity.living_entity.is_alive() {
                return;
            }
            // 先刷新世界边界快照，之后的目标判定都读同步快照（见 is_within_border）。
            self.refresh_border_snapshot().await;
            self.tick_cooldowns().await;
            self.tick_ambient_sound().await;

            let entity = &self.mob_entity.living_entity.entity;
            let age = entity.age.load(Relaxed);

            // 原版 `(tickCount + getId()) % 120 == 0` 时给周围玩家上黑暗
            // （Warden.java:299-301）；加 id 是为了让不同个体错峰。
            if age.wrapping_add(entity.entity_id) % DARKNESS_INTERVAL == 0 {
                self.apply_darkness_around().await;
            }

            // 原版每 20 tick 跑一次愤怒衰减与客户端同步（Warden.java:302-305）。
            if age % ANGERMANAGEMENT_TICK_DELAY == 0 {
                self.update_anger().await;
                self.sync_client_anger_level().await;
            }
        })
    }

    /// 原版 `Warden.hurtServer`（Warden.java:507-520）：受击时给攻击者加
    /// `ANGRY 最低值 + 20`（= 100）愤怒，直接跳过咆哮进入战斗。
    fn on_damage<'a>(
        &'a self,
        _damage_type: DamageType,
        source: Option<&'a dyn EntityBase>,
    ) -> EntityBaseFuture<'a, ()> {
        Box::pin(async move {
            let entity = &self.mob_entity.living_entity.entity;
            // 原版 `!isNoAi() && !isDiggingOrEmerging()`。
            if self.mob_entity.is_no_ai()
                || matches!(
                    entity.pose.load(),
                    EntityPose::Digging | EntityPose::Emerging
                )
            {
                return;
            }
            let Some(attacker) = source else {
                return;
            };
            let attacker_uuid = attacker.get_entity().entity_uuid;
            self.increase_anger_at_uuid(
                attacker_uuid,
                AngerLevel::Angry.minimum_anger() + ON_HURT_ANGER_BOOST,
                false,
            )
            .await;

            // 原版：还没有攻击目标且攻击者是生物时，直接锁定（近战或 5 格内）。
            if self.mob_entity.target.lock().await.is_some() {
                return;
            }
            if attacker.get_living_entity().is_none() {
                return;
            }
            let within_reach = entity
                .pos
                .load()
                .squared_distance_to_vec(&attacker.get_entity().pos.load())
                <= 5.0 * 5.0;
            if within_reach {
                self.set_attack_target_by_uuid(attacker_uuid).await;
            }
        })
    }

    /// 原版 `Warden.doPush`（Warden.java:544-551）：被实体挤到时加满 35 点愤怒，
    /// 并把对方位置记为骚动位置，20 tick 冷却一次。
    fn mob_player_collision<'a>(
        &'a self,
        player: &'a Arc<crate::entity::player::Player>,
    ) -> EntityBaseFuture<'a, ()> {
        Box::pin(async move {
            if self.mob_entity.is_no_ai() || self.touch_cooldown.load(Relaxed) > 0 {
                return;
            }
            self.touch_cooldown.store(TOUCH_COOLDOWN_TICKS, Relaxed);
            let uuid = player.living_entity.entity.entity_uuid;
            self.increase_anger_at_uuid(uuid, DEFAULT_ANGER, true).await;
            self.set_disturbance_location(player.living_entity.entity.pos.load().to_block_pos())
                .await;
        })
    }

    /// 原版 `Warden.setAttackTarget` 的对外入口：任何外部设目标的路径都要压
    /// 音爆冷却并清咆哮目标（Warden.java:522-527）。
    fn set_mob_target(&self, target: Option<Arc<dyn EntityBase>>) -> EntityBaseFuture<'_, ()> {
        Box::pin(async move {
            if let Some(target) = target {
                self.set_attack_target(target).await;
            } else {
                *self.mob_entity.target.lock().await = None;
            }
        })
    }
}
