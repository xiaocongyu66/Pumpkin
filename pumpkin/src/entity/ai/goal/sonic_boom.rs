//! 监守者音爆，逐行对照原版 `ai/behavior/warden/SonicBoom.java`（92 行）。
//!
//! 原版是 Brain 的 `Behavior`，靠 `SONIC_BOOM_COOLDOWN` /
//! `SONIC_BOOM_SOUND_DELAY` / `SONIC_BOOM_SOUND_COOLDOWN` 三个 MemoryModule
//! 计时；Pumpkin 没有 Brain，这里把「蓄力延迟」与「一次只发一发」两个计时器
//! 变成 Goal 自身的字段，而冷却放在
//! `WardenEntity::sonic_boom_cooldown` 上——因为原版
//! `Warden.doHurtTarget`（Warden.java:236）与 `Warden.setAttackTarget`
//! （Warden.java:526）都要从行为之外写入它，对应原版
//! `SonicBoom.setCooldown`（SonicBoom.java:88-90）。

use std::sync::Weak;

use pumpkin_data::damage::DamageType;
use pumpkin_data::entity::EntityStatus;
use pumpkin_data::particle::Particle;
use pumpkin_data::sound::{Sound, SoundCategory};
use pumpkin_util::math::vector3::Vector3;

use super::{Controls, Goal, GoalFuture};
use crate::entity::mob::Mob;
use crate::entity::mob::warden::WardenEntity;

/// 原版 `SonicBoom.DISTANCE_XZ`（SonicBoom.java:28）。
const DISTANCE_XZ: f64 = 15.0;

/// 原版 `SonicBoom.DISTANCE_Y`（SonicBoom.java:29）。
const DISTANCE_Y: f64 = 20.0;

/// 原版 `SonicBoom.KNOCKBACK_VERTICAL`（SonicBoom.java:30）。
const KNOCKBACK_VERTICAL: f64 = 0.5;

/// 原版 `SonicBoom.KNOCKBACK_HORIZONTAL`（SonicBoom.java:31）。
const KNOCKBACK_HORIZONTAL: f64 = 2.5;

/// 原版 `SonicBoom.COOLDOWN`（SonicBoom.java:32）：行为结束后 40 tick 内不再音爆。
pub const COOLDOWN: i32 = 40;

/// 原版 `SonicBoom.TICKS_BEFORE_PLAYING_SOUND`（SonicBoom.java:33）：`Mth.ceil(34.0)`。
const TICKS_BEFORE_PLAYING_SOUND: i32 = 34;

/// 原版 `SonicBoom.DURATION`（SonicBoom.java:34）：`Mth.ceil(60.0f)`。
const DURATION: i32 = 60;

/// 原版音爆伤害 10.0（SonicBoom.java:75），伤害类型 `DamageTypes.SONIC_BOOM`
/// 带 `bypasses_armor` 标签，因此穿透护甲。
const BOOM_DAMAGE: f32 = 10.0;

/// 原版 `EntityType.WARDEN` 的 `EntityAttachment.WARDEN_CHEST` 偏移
/// `(0.0f, 1.6f, 0.0f)`（EntityTypes.java:308）——音爆光柱的起点就是胸腔位置。
const WARDEN_CHEST_OFFSET_Y: f64 = 1.6;

/// 原版音爆粒子步数补正：`Mth.floor(delta.length()) + 7`（SonicBoom.java:69）。
const PARTICLE_STEP_BONUS: i32 = 7;

pub struct SonicBoomGoal {
    warden: Weak<WardenEntity>,
    /// 原版 Behavior 基类的运行计时，上限 `DURATION`。
    ticks: i32,
    /// 原版 `MemoryModuleType.SONIC_BOOM_SOUND_COOLDOWN`（SonicBoom.java:64）：
    /// 一次行为内只发射一发音爆。
    fired: bool,
}

impl SonicBoomGoal {
    #[must_use]
    pub const fn new(warden: Weak<WardenEntity>) -> Box<Self> {
        Box::new(Self {
            warden,
            ticks: 0,
            fired: false,
        })
    }

    /// 原版 `closerThan(entity, 15.0, 20.0)`：XZ 与 Y 分开判定的圆柱体
    /// （SonicBoom.java:42、65）。
    fn within_boom_range(from: Vector3<f64>, to: Vector3<f64>) -> bool {
        let dx = to.x - from.x;
        let dz = to.z - from.z;
        dx.mul_add(dx, dz * dz) <= DISTANCE_XZ * DISTANCE_XZ && (to.y - from.y).abs() <= DISTANCE_Y
    }
}

impl Goal for SonicBoomGoal {
    fn can_start<'a>(&'a mut self, mob: &'a dyn Mob) -> GoalFuture<'a, bool> {
        Box::pin(async move {
            // 原版 `MemoryModuleType.SONIC_BOOM_COOLDOWN` 存在时不启动
            // （SonicBoom.java:37）。冷却由监守者实体持有，因为
            // `Warden.doHurtTarget`（Warden.java:236）与 `setAttackTarget`
            // （Warden.java:526）都要从外部写入它。
            let Some(warden) = self.warden.upgrade() else {
                return false;
            };
            if warden
                .sonic_boom_cooldown
                .load(std::sync::atomic::Ordering::Relaxed)
                > 0
            {
                return false;
            }
            // 原版 `checkExtraStartConditions`：需要 ATTACK_TARGET 且在 15×20 格内
            // （SonicBoom.java:41-43）。
            let target = mob.get_mob_entity().target.lock().await;
            let Some(target) = target.as_ref() else {
                return false;
            };
            if !target.get_entity().is_alive() {
                return false;
            }
            Self::within_boom_range(mob.get_entity().pos.load(), target.get_entity().pos.load())
        })
    }

    fn should_continue<'a>(&'a self, _mob: &'a dyn Mob) -> GoalFuture<'a, bool> {
        // 原版 `canStillUse` 恒为 true（SonicBoom.java:46-48），只受 duration 限制。
        Box::pin(async move { self.ticks < DURATION })
    }

    fn start<'a>(&'a mut self, mob: &'a dyn Mob) -> GoalFuture<'a, ()> {
        Box::pin(async move {
            self.ticks = 0;
            self.fired = false;
            // 原版 `start`：广播实体事件 62（客户端播蓄力动画）并播 `WARDEN_SONIC_CHARGE`
            // ，音量 3.0（SonicBoom.java:52-56）。
            let entity = mob.get_entity();
            let world = entity.world.load();
            world.send_entity_status(entity, EntityStatus::SonicCharge);
            world.play_sound_fine(
                Sound::EntityWardenSonicCharge,
                SoundCategory::Hostile,
                &entity.pos.load(),
                3.0,
                1.0,
            );
        })
    }

    fn should_run_every_tick(&self) -> bool {
        true
    }

    fn tick<'a>(&'a mut self, mob: &'a dyn Mob) -> GoalFuture<'a, ()> {
        Box::pin(async move {
            let target = mob.get_mob_entity().target.lock().await.clone();
            let Some(target) = target else {
                return;
            };

            // 原版每 tick 都盯着目标（SonicBoom.java:60）。
            {
                let target_pos = target.get_entity().pos.load();
                mob.get_mob_entity()
                    .look_control
                    .lock()
                    .unwrap()
                    .look_at_with_range(target_pos.x, target_pos.y, target_pos.z, 30.0, 30.0);
            }

            self.ticks += 1;
            // 原版靠 `SONIC_BOOM_SOUND_DELAY` 等 34 tick，再靠
            // `SONIC_BOOM_SOUND_COOLDOWN` 保证只发一次（SonicBoom.java:61-64）。
            if self.fired || self.ticks < TICKS_BEFORE_PLAYING_SOUND {
                return;
            }
            self.fired = true;

            // 原版发射前重新校验目标可攻击且仍在 15×20 格内（SonicBoom.java:65）。
            let entity = mob.get_entity();
            let source_pos = entity.pos.load();
            let target_pos = target.get_entity().pos.load();
            let still_valid = self
                .warden
                .upgrade()
                .is_some_and(|warden| warden.can_target_entity(target.as_ref()));
            if !still_valid || !Self::within_boom_range(source_pos, target_pos) {
                return;
            }

            // 原版光柱起点：监守者位置 + WARDEN_CHEST 挂点（SonicBoom.java:66）。
            let source = Vector3::new(
                source_pos.x,
                source_pos.y + WARDEN_CHEST_OFFSET_Y,
                source_pos.z,
            );
            // 原版终点是目标的眼睛位置（SonicBoom.java:67）。
            let delta = target.get_entity().get_eye_pos().sub(&source);
            let normalize = delta.normalize();
            // 原版 `Mth.floor(delta.length()) + 7` 步，i 从 1 到 steps-1
            // （SonicBoom.java:69-73）：每格一颗 SonicBoom 粒子，末端多出 6 颗
            // 越过目标，形成贯穿的光柱。
            let steps = delta.length().floor() as i32 + PARTICLE_STEP_BONUS;
            let world = entity.world.load();
            for i in 1..steps {
                let particle_pos =
                    source.add(&normalize.multiply(f64::from(i), f64::from(i), f64::from(i)));
                // 原版 `sendParticles(SONIC_BOOM, x, y, z, 1, 0.0, 0.0, 0.0, 0.0)`：
                // 单颗、零偏移、零速度。
                world.spawn_particle(
                    particle_pos,
                    Vector3::new(0.0, 0.0, 0.0),
                    0.0,
                    1,
                    Particle::SonicBoom,
                );
            }

            // 原版 `playSound(WARDEN_SONIC_BOOM, 3.0f, 1.0f)`（SonicBoom.java:74）。
            world.play_sound_fine(
                Sound::EntityWardenSonicBoom,
                SoundCategory::Hostile,
                &source_pos,
                3.0,
                1.0,
            );

            // 原版 10.0 点 `sonicBoom` 伤害（SonicBoom.java:75）；该伤害类型带
            // `bypasses_armor`，所以护甲不减伤。
            let attacker = self.warden.upgrade();
            let attacker_ref = attacker.as_ref().map(|warden| {
                let base: &dyn crate::entity::EntityBase = warden.as_ref();
                base
            });
            let hurt = target
                .damage_with_context(
                    target.as_ref(),
                    BOOM_DAMAGE,
                    DamageType::SONIC_BOOM,
                    None,
                    attacker_ref,
                    attacker_ref,
                )
                .await;
            if !hurt {
                return;
            }

            // 原版击退按目标的击退抗性缩放（SonicBoom.java:76-78）：
            // 垂直 0.5、水平 2.5，沿光柱方向推。
            let resistance = target.get_living_entity().map_or(0.0, |living| {
                living.get_attribute_value(
                    &pumpkin_data::attributes::Attributes::KNOCKBACK_RESISTANCE,
                )
            });
            let vertical = KNOCKBACK_VERTICAL * (1.0 - resistance);
            let horizontal = KNOCKBACK_HORIZONTAL * (1.0 - resistance);
            target.get_entity().add_velocity(Vector3::new(
                normalize.x * horizontal,
                normalize.y * vertical,
                normalize.z * horizontal,
            ));
        })
    }

    fn stop<'a>(&'a mut self, _mob: &'a dyn Mob) -> GoalFuture<'a, ()> {
        Box::pin(async move {
            // 原版 `stop` 设 40 tick 冷却（SonicBoom.java:84-86）。
            if let Some(warden) = self.warden.upgrade() {
                warden
                    .sonic_boom_cooldown
                    .store(COOLDOWN, std::sync::atomic::Ordering::Relaxed);
            }
        })
    }

    fn controls(&self) -> Controls {
        Controls::MOVE | Controls::LOOK
    }
}
