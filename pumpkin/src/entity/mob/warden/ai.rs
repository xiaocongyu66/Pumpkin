//! 监守者行为，对应原版 `WardenAi.java`（142 行）编排的
//! `ai/behavior/warden/` 下各 `Behavior`。
//!
//! 原版用 Brain + Activity + MemoryModule 调度；Pumpkin 没有 Brain，这里把每个
//! Behavior 映射成一个 `Goal`，用 `GoalSelector` 的优先级复现原版
//! `WardenAi.updateActivity`（WardenAi.java:78-80）的活动优先级：
//! `ROAR(咆哮) > FIGHT(战斗) > INVESTIGATE(调查) > SNIFF(嗅探) > IDLE(闲逛)`。
//!
//! 原版 `Activity.EMERGE`/`Activity.DIG`（出土与钻地）依赖生成钩子与
//! `WardenSpawnTracker`，Pumpkin 尚无对应基础设施，未实现。

use std::sync::Weak;

use pumpkin_data::entity::EntityPose;
use pumpkin_data::sound::{Sound, SoundCategory};
use pumpkin_util::math::vector3::Vector3;

use super::WardenEntity;
use crate::entity::ai::goal::{Controls, Goal, GoalFuture};
use crate::entity::ai::pathfinder::NavigatorGoal;
use crate::entity::mob::Mob;

/// 原版 `WardenAi.ROAR_DURATION`（WardenAi.java:63）：`Mth.ceil(84.0f)`。
pub const ROAR_DURATION: i32 = 84;

/// 原版 `Roar.TICKS_BEFORE_PLAYING_ROAR_SOUND`（Roar.java:26）。
const TICKS_BEFORE_PLAYING_ROAR_SOUND: i32 = 25;

/// 原版 `Roar.ROAR_ANGER_INCREASE`（Roar.java:27）。
const ROAR_ANGER_INCREASE: i32 = 20;

/// 原版 `WardenAi.SNIFFING_DURATION`（WardenAi.java:64）：`Mth.ceil(83.2f)`。
const SNIFFING_DURATION: i32 = 84;

/// 原版 `WardenAi.SPEED_MULTIPLIER_WHEN_INVESTIGATING`（WardenAi.java:58）。
const SPEED_MULTIPLIER_WHEN_INVESTIGATING: f64 = 0.7;

/// 原版 `Sniffing.ANGER_FROM_SNIFFING_MAX_DISTANCE_XZ`（Sniffing.java:23）。
const ANGER_FROM_SNIFFING_MAX_DISTANCE_XZ: f64 = 6.0;

/// 原版 `Sniffing.ANGER_FROM_SNIFFING_MAX_DISTANCE_Y`（Sniffing.java:24）。
const ANGER_FROM_SNIFFING_MAX_DISTANCE_Y: f64 = 20.0;

/// 原版 `TryToSniff` 的嗅探冷却（TryToSniff.java）与
/// `WardenAi.DISTURBANCE_LOCATION_EXPIRY_TIME`（WardenAi.java:66）同为 100 tick。
const SNIFF_COOLDOWN: i32 = 100;

/// 原版 `Roar`（Roar.java:24-66）+ `SetRoarTarget`（SetRoarTarget.java:18-30）。
///
/// 对应原版 `Activity.ROAR`（WardenAi.java:106-108），优先级高于 FIGHT：
/// 监守者锁定目标前先站定咆哮 `ROAR_DURATION` tick，结束时把咆哮目标转为攻击目标
/// （Roar.java:58-65 的 `stop`）。
pub struct RoarGoal {
    warden: Weak<WardenEntity>,
    /// 原版由 Behavior 基类的 `duration` 计时；这里自己数 tick。
    ticks: i32,
    /// 原版 `MemoryModuleType.ROAR_SOUND_COOLDOWN`（Roar.java:51-55）：
    /// 一次咆哮只播一次音效。
    played_sound: bool,
}

impl RoarGoal {
    #[must_use]
    pub const fn new(warden: Weak<WardenEntity>) -> Box<Self> {
        Box::new(Self {
            warden,
            ticks: 0,
            played_sound: false,
        })
    }
}

impl Goal for RoarGoal {
    fn can_start<'a>(&'a mut self, _mob: &'a dyn Mob) -> GoalFuture<'a, bool> {
        Box::pin(async move {
            let Some(warden) = self.warden.upgrade() else {
                return false;
            };
            // 原版 SetRoarTarget 要求 ATTACK_TARGET 缺失（SetRoarTarget.java:20）。
            if warden.mob_entity.target.lock().await.is_some() {
                return false;
            }
            warden.roar_target.lock().await.is_some()
        })
    }

    fn should_continue<'a>(&'a self, _mob: &'a dyn Mob) -> GoalFuture<'a, bool> {
        // 原版 `Roar.canStillUse` 恒为 true（Roar.java:44-47），只受 duration 限制。
        Box::pin(async move { self.ticks < ROAR_DURATION })
    }

    fn start<'a>(&'a mut self, mob: &'a dyn Mob) -> GoalFuture<'a, ()> {
        Box::pin(async move {
            self.ticks = 0;
            self.played_sound = false;
            let Some(warden) = self.warden.upgrade() else {
                return;
            };
            // 原版 `Roar.start`：清除行走目标并摆出咆哮姿态（Roar.java:34-42）。
            mob.get_mob_entity().navigator.lock().unwrap().stop();
            let entity = &warden.mob_entity.living_entity.entity;
            entity.set_pose(EntityPose::Roaring);

            let roar_target = *warden.roar_target.lock().await;
            if let Some(target_uuid) = roar_target {
                let world = entity.world.load();
                if let Some(target) = world.get_entity_by_uuid(target_uuid) {
                    let eye = target.get_entity().get_eye_pos();
                    warden
                        .mob_entity
                        .look_control
                        .lock()
                        .unwrap()
                        .look_at_with_range(eye.x, eye.y, eye.z, 30.0, 30.0);
                }
                // 原版 `body.increaseAngerAt(target, 20, false)`（Roar.java:41）。
                warden
                    .increase_anger_at_uuid(target_uuid, ROAR_ANGER_INCREASE, false)
                    .await;
            }
        })
    }

    fn should_run_every_tick(&self) -> bool {
        true
    }

    fn tick<'a>(&'a mut self, _mob: &'a dyn Mob) -> GoalFuture<'a, ()> {
        Box::pin(async move {
            self.ticks += 1;
            // 原版 `MemoryModuleType.ROAR_SOUND_DELAY` 为 25 tick（Roar.java:36），
            // 延迟结束后播一次 `WARDEN_ROAR`（Roar.java:51-55）。
            if self.played_sound || self.ticks < TICKS_BEFORE_PLAYING_ROAR_SOUND {
                return;
            }
            self.played_sound = true;
            if let Some(warden) = self.warden.upgrade() {
                let entity = &warden.mob_entity.living_entity.entity;
                entity.world.load().play_sound_fine(
                    Sound::EntityWardenRoar,
                    SoundCategory::Hostile,
                    &entity.pos.load(),
                    3.0,
                    1.0,
                );
            }
        })
    }

    fn stop<'a>(&'a mut self, _mob: &'a dyn Mob) -> GoalFuture<'a, ()> {
        Box::pin(async move {
            let Some(warden) = self.warden.upgrade() else {
                return;
            };
            let entity = &warden.mob_entity.living_entity.entity;
            if entity.pose.load() == EntityPose::Roaring {
                entity.set_pose(EntityPose::Standing);
            }
            // 原版 `Roar.stop`：咆哮目标升级为攻击目标后清空（Roar.java:63-64）。
            let roar_target = warden.roar_target.lock().await.take();
            if let Some(target_uuid) = roar_target {
                warden.set_attack_target_by_uuid(target_uuid).await;
            }
        })
    }

    fn controls(&self) -> Controls {
        // 只占 MOVE|LOOK：原版 Roar 属于 ROAR 活动，而 Swim 在 CORE 活动里始终并行
        // （WardenAi.java:83）。这里若一并占用 JUMP，就会和优先级更高的 SwimGoal
        // 互斥，导致监守者在水里无法咆哮。
        Controls::MOVE | Controls::LOOK
    }
}

/// 原版 `Activity.INVESTIGATE`（WardenAi.java:98-100）：
/// `GoToTargetLocation.create(MemoryModuleType.DISTURBANCE_LOCATION, 2, 0.7f)`。
///
/// 这是「听到声音后快速移动」的落地环节：`disturbance_location` 由振动接收
/// （原版 `WardenAi.setDisturbanceLocation`，WardenAi.java:131-140）写入，
/// 本 Goal 负责朝它走过去。
pub struct InvestigateDisturbanceGoal {
    warden: Weak<WardenEntity>,
}

impl InvestigateDisturbanceGoal {
    #[must_use]
    pub const fn new(warden: Weak<WardenEntity>) -> Box<Self> {
        Box::new(Self { warden })
    }

    /// 原版 `Activity.INVESTIGATE` 要求 `DISTURBANCE_LOCATION` 存在，
    /// 且 ROAR/ATTACK 活动都不活跃（靠活动优先级实现）。
    async fn has_work(warden: &WardenEntity) -> bool {
        if warden.mob_entity.target.lock().await.is_some()
            || warden.roar_target.lock().await.is_some()
        {
            return false;
        }
        warden.disturbance_location.lock().await.is_some()
    }
}

impl Goal for InvestigateDisturbanceGoal {
    fn can_start<'a>(&'a mut self, _mob: &'a dyn Mob) -> GoalFuture<'a, bool> {
        Box::pin(async move {
            let Some(warden) = self.warden.upgrade() else {
                return false;
            };
            Self::has_work(&warden).await
        })
    }

    fn should_continue<'a>(&'a self, _mob: &'a dyn Mob) -> GoalFuture<'a, bool> {
        Box::pin(async move {
            let Some(warden) = self.warden.upgrade() else {
                return false;
            };
            Self::has_work(&warden).await
        })
    }

    fn should_run_every_tick(&self) -> bool {
        true
    }

    fn tick<'a>(&'a mut self, mob: &'a dyn Mob) -> GoalFuture<'a, ()> {
        Box::pin(async move {
            let Some(warden) = self.warden.upgrade() else {
                return;
            };
            let Some(disturbance) = *warden.disturbance_location.lock().await else {
                return;
            };
            let entity = &warden.mob_entity.living_entity.entity;
            let pos = entity.pos.load();
            let destination = disturbance.to_centered_f64();

            // 原版 GoToTargetLocation 的 `closeEnoughDist` 为 2 格
            // （WardenAi.java:99），到位后停下并转向声源。
            if pos.squared_distance_to_vec(&destination) <= 2.0 * 2.0 {
                mob.get_mob_entity().navigator.lock().unwrap().stop();
            } else {
                let mut navigator = mob.get_mob_entity().navigator.lock().unwrap();
                navigator.set_progress(NavigatorGoal::new(
                    pos,
                    destination,
                    SPEED_MULTIPLIER_WHEN_INVESTIGATING,
                ));
            }
            // 原版 `SetWardenLookTarget`（SetWardenLookTarget.java:21-27）：没有攻击
            // 目标时看向咆哮目标或骚动位置。
            warden
                .mob_entity
                .look_control
                .lock()
                .unwrap()
                .look_at_with_range(destination.x, destination.y, destination.z, 30.0, 30.0);
        })
    }

    fn stop<'a>(&'a mut self, mob: &'a dyn Mob) -> GoalFuture<'a, ()> {
        Box::pin(async move {
            mob.get_mob_entity().navigator.lock().unwrap().stop();
        })
    }

    fn controls(&self) -> Controls {
        Controls::MOVE | Controls::LOOK
    }
}

/// 原版 `TryToSniff`（TryToSniff.java:18-31）+ `Sniffing`（Sniffing.java:21-55）。
///
/// 对应原版 `Activity.SNIFF`（WardenAi.java:102-104）：闲置时站定嗅探，
/// 结束时对 6×20 格范围内的可攻击目标加愤怒并写入骚动位置
/// （Sniffing.java:46-53 的 `stop`）。
pub struct SniffGoal {
    warden: Weak<WardenEntity>,
    ticks: i32,
    /// 原版 `MemoryModuleType.SNIFF_COOLDOWN`（WardenAi.java:136）。
    cooldown: i32,
}

impl SniffGoal {
    #[must_use]
    pub const fn new(warden: Weak<WardenEntity>) -> Box<Self> {
        Box::new(Self {
            warden,
            ticks: 0,
            cooldown: 0,
        })
    }
}

impl Goal for SniffGoal {
    fn can_start<'a>(&'a mut self, _mob: &'a dyn Mob) -> GoalFuture<'a, bool> {
        Box::pin(async move {
            if self.cooldown > 0 {
                self.cooldown -= 1;
                return false;
            }
            let Some(warden) = self.warden.upgrade() else {
                return false;
            };
            // 原版 `Sniffing` 要求 ATTACK_TARGET 与 WALK_TARGET 都缺失
            // （Sniffing.java:27）；INVESTIGATE 优先级更高，所以骚动位置存在时不嗅探。
            if warden.mob_entity.target.lock().await.is_some()
                || warden.roar_target.lock().await.is_some()
                || warden.disturbance_location.lock().await.is_some()
            {
                return false;
            }
            true
        })
    }

    fn should_continue<'a>(&'a self, _mob: &'a dyn Mob) -> GoalFuture<'a, bool> {
        // 原版 `Sniffing.canStillUse` 恒为 true（Sniffing.java:31-33）。
        Box::pin(async move { self.ticks < SNIFFING_DURATION })
    }

    fn start<'a>(&'a mut self, mob: &'a dyn Mob) -> GoalFuture<'a, ()> {
        Box::pin(async move {
            self.ticks = 0;
            let Some(warden) = self.warden.upgrade() else {
                return;
            };
            mob.get_mob_entity().navigator.lock().unwrap().stop();
            let entity = &warden.mob_entity.living_entity.entity;
            entity.set_pose(EntityPose::Sniffing);
            // 原版 `Sniffing.start` 播 `WARDEN_SNIFF`，音量 5.0（Sniffing.java:37）。
            entity.world.load().play_sound_fine(
                Sound::EntityWardenSniff,
                SoundCategory::Hostile,
                &entity.pos.load(),
                5.0,
                1.0,
            );
        })
    }

    fn should_run_every_tick(&self) -> bool {
        true
    }

    fn tick<'a>(&'a mut self, _mob: &'a dyn Mob) -> GoalFuture<'a, ()> {
        Box::pin(async move {
            self.ticks += 1;
        })
    }

    fn stop<'a>(&'a mut self, _mob: &'a dyn Mob) -> GoalFuture<'a, ()> {
        Box::pin(async move {
            self.cooldown = SNIFF_COOLDOWN;
            let Some(warden) = self.warden.upgrade() else {
                return;
            };
            let entity = &warden.mob_entity.living_entity.entity;
            if entity.pose.load() == EntityPose::Sniffing {
                entity.set_pose(EntityPose::Standing);
            }

            // 原版 `Sniffing.stop`：取 `NEAREST_ATTACKABLE`，在 6×20 格内就加愤怒，
            // 并在没有骚动位置时写入其位置（Sniffing.java:46-53）。
            let pos = entity.pos.load();
            let world = entity.world.load();
            let mut nearest: Option<(f64, uuid::Uuid, Vector3<f64>)> = None;
            for candidate in world
                .get_nearby_entities(pos, ANGER_FROM_SNIFFING_MAX_DISTANCE_Y)
                .values()
            {
                if !warden.can_target_entity(candidate.as_ref()) {
                    continue;
                }
                let candidate_pos = candidate.get_entity().pos.load();
                // 原版 `closerThan(entity, 6.0, 20.0)` 是 XZ 与 Y 分开判定的圆柱体。
                let dx = candidate_pos.x - pos.x;
                let dz = candidate_pos.z - pos.z;
                let horizontal_sq = dx.mul_add(dx, dz * dz);
                if horizontal_sq
                    > ANGER_FROM_SNIFFING_MAX_DISTANCE_XZ * ANGER_FROM_SNIFFING_MAX_DISTANCE_XZ
                    || (candidate_pos.y - pos.y).abs() > ANGER_FROM_SNIFFING_MAX_DISTANCE_Y
                {
                    continue;
                }
                if nearest
                    .as_ref()
                    .is_none_or(|(best, _, _)| horizontal_sq < *best)
                {
                    nearest = Some((
                        horizontal_sq,
                        candidate.get_entity().entity_uuid,
                        candidate_pos,
                    ));
                }
            }
            if let Some((_, uuid, candidate_pos)) = nearest {
                warden
                    .increase_anger_at_uuid(uuid, super::DEFAULT_ANGER, true)
                    .await;
                warden
                    .set_disturbance_location(candidate_pos.to_block_pos())
                    .await;
            }
        })
    }

    fn controls(&self) -> Controls {
        Controls::MOVE | Controls::LOOK
    }
}
