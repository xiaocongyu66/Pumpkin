//! 监守者的振动感知，对应原版 `Warden.VibrationUser` 内部类
//! （Warden.java:594-667）与 `VibrationSystem.User` 接口。
//!
//! 原版监守者实现 `VibrationSystem`，通过 `DynamicGameEventListener` 注册到区块的
//! 事件注册表，由 `Level.gameEvent` 派发；Pumpkin 的
//! [`crate::world::vibrations`] 目前只服务幽匿感测体，因此这里改为在
//! [`WardenEntity::dispatch_vibration`] 中按距离扫描听力范围内的监守者。

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use pumpkin_data::entity::{EntityPose, EntityStatus, EntityType};
use pumpkin_data::sound::{Sound, SoundCategory};
use pumpkin_util::math::vector3::Vector3;
use uuid::Uuid;

use super::{
    DEFAULT_ANGER, GAME_EVENT_LISTENER_RANGE, PROJECTILE_ANGER, PROJECTILE_ANGER_DISTANCE,
    VIBRATION_COOLDOWN_TICKS, WardenEntity,
};
use crate::entity::EntityBase;
use crate::world::vibrations::Vibration;

/// 振动接收链路，对应原版 `Warden.VibrationUser`（Warden.java:594-667）。
impl WardenEntity {
    /// 原版 `VibrationUser.canReceiveVibration`（Warden.java:628-635）。
    ///
    /// 全部判定都是同步的（原子量 + 世界边界），所以这里不做成 async。
    fn can_receive_vibration(&self, source: Vector3<f64>) -> bool {
        let entity = &self.mob_entity.living_entity.entity;
        if self.mob_entity.is_no_ai() || !self.mob_entity.living_entity.is_alive() {
            return false;
        }
        // 原版 `hasMemoryValue(VIBRATION_COOLDOWN)`。
        if self.vibration_cooldown.load(Relaxed) > 0 {
            return false;
        }
        // 原版 `isDiggingOrEmerging()`：出土/钻地期间不听。
        if matches!(
            entity.pose.load(),
            EntityPose::Digging | EntityPose::Emerging
        ) {
            return false;
        }
        // 原版 `getWorldBorder().isWithinBounds(pos)`。
        self.is_within_border(source)
    }

    /// 原版 `VibrationUser.onReceiveVibration`（Warden.java:638-667）。
    ///
    /// 这是「感知振动 → 增加愤怒 → 朝声源移动/咆哮 → 达到阈值设为攻击目标」
    /// 链路的入口：
    /// 1. 压 40 tick 振动冷却，广播触须抖动事件 61，播 `WARDEN_TENDRIL_CLICKS`；
    /// 2. 给声源实体加 `DEFAULT_ANGER`（35）愤怒（抛射物走
    ///    `PROJECTILE_ANGER` 10 点的弱化路径）；
    /// 3. 尚未 ANGRY 时写入骚动位置，交给 [`super::ai::InvestigateDisturbanceGoal`] 走过去；
    /// 4. 愤怒累到 ANGRY（80）后，[`Self::update_anger`] 会把它升级为咆哮目标，
    ///    咆哮结束再转成攻击目标。
    pub async fn on_receive_vibration(
        &self,
        source: Vector3<f64>,
        source_entity: Option<Uuid>,
        projectile_owner: Option<Uuid>,
    ) {
        if !self.can_receive_vibration(source) {
            return;
        }
        let entity = &self.mob_entity.living_entity.entity;
        let world = entity.world.load();

        // 原版：40 tick 振动冷却 + 实体事件 61 + 触须音效（音量 5.0）。
        self.vibration_cooldown
            .store(VIBRATION_COOLDOWN_TICKS, Relaxed);
        world.send_entity_status(entity, EntityStatus::TendrilsShiver);
        world.play_sound_fine(
            Sound::EntityWardenTendrilClicks,
            SoundCategory::Hostile,
            &entity.pos.load(),
            5.0,
            1.0,
        );

        let mut suspicious_pos = source.to_block_pos();
        if let Some(owner_uuid) = projectile_owner {
            // 原版抛射物分支（Warden.java:646-657）：射手在 30 格内才记恨。
            // 原版靠 `MemoryModuleType.RECENT_PROJECTILE` 区分首发与连发：首发只加
            // 10 点，短时间内再来才加满 35 点并把可疑位置改成射手位置。Pumpkin 没有
            // 该 memory，这里统一走首发的 10 点弱化路径，宁可保守。
            if let Some(owner) = world.get_entity_by_uuid(owner_uuid) {
                let owner_pos = owner.get_entity().pos.load();
                if entity.pos.load().squared_distance_to_vec(&owner_pos)
                    <= PROJECTILE_ANGER_DISTANCE * PROJECTILE_ANGER_DISTANCE
                {
                    if self.can_target_entity(owner.as_ref()) {
                        suspicious_pos = owner_pos.to_block_pos();
                    }
                    self.increase_anger_at_uuid(owner_uuid, PROJECTILE_ANGER, true)
                        .await;
                }
            }
        } else if let Some(uuid) = source_entity {
            // 原版 `increaseAngerAt(sourceEntity)` 走默认 35 点（Warden.java:659、459-461）。
            self.increase_anger_at_uuid(uuid, DEFAULT_ANGER, true).await;
        }

        // 原版：还没到 ANGRY 时才去调查声源（Warden.java:661-666）。已经愤怒就该
        // 直接咆哮/攻击，不再被杂音牵走。
        if self.anger_level().await.is_angry() {
            return;
        }
        let top_suspect = {
            let anger = self.anger_management.lock().await;
            anger.top_suspect(&|uuid| self.can_target_uuid(uuid))
        };
        // 原版条件：抛射物、或当前没有嫌疑对象、或嫌疑对象正是这次的声源。
        if projectile_owner.is_some() || top_suspect.is_none() || top_suspect == source_entity {
            self.set_disturbance_location(suspicious_pos).await;
        }
    }

    /// 世界侧振动分发的入口：把一次 [`Vibration`] 投递给听力范围内的监守者。
    ///
    /// 原版是 `DynamicGameEventListener` 把监守者注册进区块的 `GameEventListenerRegistry`，
    /// 由 `Level.gameEvent` 统一派发；Pumpkin 的 [`crate::world::vibrations`] 目前只
    /// 服务幽匿感测体，所以这里提供一个按距离扫描的等价入口（听力半径同为 16 格，
    /// 对应原版 `VibrationUser.getListenerRadius`，Warden.java:608-610）。
    pub async fn dispatch_vibration(
        world: &Arc<crate::world::World>,
        event: Vibration,
        source: Vector3<f64>,
        source_entity: Option<Uuid>,
        projectile_owner: Option<Uuid>,
    ) {
        // 原版靠 `GameEventTags.WARDEN_CAN_LISTEN` 过滤可听事件
        // （Warden.java:618-620）。频率为 0 的事件不在振动表内，直接忽略。
        if event.frequency() == 0 {
            return;
        }
        // 先按实体类型过滤再算距离：振动事件相当频繁，而监守者极少见，
        // 这样绝大多数世界只需扫一遍实体表就能直接跳过。
        let listener_range_sq = GAME_EVENT_LISTENER_RANGE * GAME_EVENT_LISTENER_RANGE;
        let candidates: Vec<Arc<dyn EntityBase>> = world
            .entities
            .load()
            .iter()
            .filter(|entity| {
                let base = entity.get_entity();
                base.entity_type.id == EntityType::WARDEN.id
                    && base.pos.load().squared_distance_to_vec(&source) <= listener_range_sq
            })
            .cloned()
            .collect();
        for candidate in candidates {
            if let Some(warden) = candidate.cast_any().downcast_ref::<Self>() {
                warden
                    .on_receive_vibration(source, source_entity, projectile_owner)
                    .await;
            }
        }
    }
}
