//! 监守者愤怒管理，对应原版 `AngerLevel.java`（55 行）与
//! `AngerManagement.java`（200 行）。
//!
//! 与原版的唯一结构差异：原版用 `angerBySuspect`（实体引用）+ `angerByUuid`
//! （已卸载实体）两张表，并靠 `conversionDelay` 在两者间转换
//! （`AngerManagement.java:84-143`）。Pumpkin 里实体身份本身就是 UUID，
//! 两张表合并为一张即可，转换延迟随之取消。衰减、上限、排序规则逐项照抄。

use std::collections::HashMap;

use pumpkin_data::sound::Sound;
use uuid::Uuid;

/// 原版 `AngerLevel`（AngerLevel.java:11-14）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AngerLevel {
    Calm,
    Agitated,
    Angry,
}

impl AngerLevel {
    /// 原版 `AngerLevel.getMinimumAnger`（AngerLevel.java:27-29）。
    #[must_use]
    pub const fn minimum_anger(self) -> i32 {
        match self {
            Self::Calm => 0,
            Self::Agitated => 40,
            Self::Angry => 80,
        }
    }

    /// 原版 `AngerLevel.getAmbientSound`（AngerLevel.java:31-33）。
    #[must_use]
    pub const fn ambient_sound(self) -> Sound {
        match self {
            Self::Calm => Sound::EntityWardenAmbient,
            Self::Agitated => Sound::EntityWardenAgitated,
            Self::Angry => Sound::EntityWardenAngry,
        }
    }

    /// 原版 `AngerLevel.getListeningSound`（AngerLevel.java:35-37）。
    #[must_use]
    pub const fn listening_sound(self) -> Sound {
        match self {
            Self::Calm => Sound::EntityWardenListening,
            Self::Agitated | Self::Angry => Sound::EntityWardenListeningAngry,
        }
    }

    /// 原版 `AngerLevel.byAnger`（AngerLevel.java:39-45）：按 `minimumAnger`
    /// 降序找第一个满足 `anger >= minimumAnger` 的等级。
    #[must_use]
    pub const fn by_anger(anger: i32) -> Self {
        if anger >= Self::Angry.minimum_anger() {
            Self::Angry
        } else if anger >= Self::Agitated.minimum_anger() {
            Self::Agitated
        } else {
            Self::Calm
        }
    }

    /// 原版 `AngerLevel.isAngry`（AngerLevel.java:47-49）。
    #[must_use]
    pub const fn is_angry(self) -> bool {
        matches!(self, Self::Angry)
    }
}

/// 原版 `AngerManagement.MAX_ANGER`（AngerManagement.java:53）。
pub const MAX_ANGER: i32 = 150;

/// 原版 `AngerManagement.DEFAULT_ANGER_DECREASE`（AngerManagement.java:54）。
const DEFAULT_ANGER_DECREASE: i32 = 1;

/// 一个嫌疑对象的快照：UUID + 当前愤怒值 + 是否玩家。
///
/// 原版排序器 `AngerManagement.Sorter`（AngerManagement.java:176-198）需要知道
/// 目标是不是玩家（玩家优先），所以这里把它缓存下来，避免排序时反查世界。
#[derive(Clone, Copy)]
struct Suspect {
    uuid: Uuid,
    anger: i32,
    is_player: bool,
}

/// 原版 `AngerManagement`（AngerManagement.java:49-198）。
///
/// 不含原版的 `filter`（`Predicate<Entity>`）字段：Pumpkin 侧调用方
/// （[`super::WardenEntity`]）在 tick 时把判定结果传进来，避免在这里持有
/// 指向监守者自身的回环引用。
#[derive(Default)]
pub struct AngerManagement {
    /// 原版 `angerBySuspect` 与 `angerByUuid` 的合并表（AngerManagement.java:63-65）。
    anger_by_suspect: HashMap<Uuid, i32>,
    /// 原版 `suspects`（AngerManagement.java:60），已按 `Sorter` 排序。
    suspects: Vec<Suspect>,
    /// 原版 `highestAnger`（AngerManagement.java:56）。
    highest_anger: i32,
}

impl AngerManagement {
    /// 原版 `AngerManagement.increaseAnger`（AngerManagement.java:145-155）：
    /// 累加后夹到 `MAX_ANGER`，返回新值。
    pub fn increase_anger(&mut self, uuid: Uuid, is_player: bool, increment: i32) -> i32 {
        let entry = self.anger_by_suspect.entry(uuid).or_insert(0);
        *entry = (*entry + increment).min(MAX_ANGER);
        let current = *entry;
        if let Some(existing) = self.suspects.iter_mut().find(|s| s.uuid == uuid) {
            existing.anger = current;
            existing.is_player = is_player;
        } else {
            self.suspects.push(Suspect {
                uuid,
                anger: current,
                is_player,
            });
        }
        self.sort_and_update_highest_anger();
        current
    }

    /// 原版 `AngerManagement.clearAnger`（AngerManagement.java:157-161）。
    pub fn clear_anger(&mut self, uuid: Uuid) {
        self.anger_by_suspect.remove(&uuid);
        self.suspects.retain(|s| s.uuid != uuid);
        self.sort_and_update_highest_anger();
    }

    /// 原版 `AngerManagement.tick`（AngerManagement.java:84-122）的衰减部分：
    /// 每次调用（原版由 `Warden.customServerAiStep` 每 20 tick 触发一次，
    /// Warden.java:302-305）给每个嫌疑对象扣 1 点；降到 <= 1 或判定失效就移除。
    ///
    /// `is_valid` 对应原版的 `validEntity` 谓词（`Warden::canTargetEntity`）：
    /// 返回 `false` 的嫌疑对象直接出表。原版还会在实体因换维度/区块卸载而消失时
    /// 把愤怒转存进 `angerByUuid`（AngerManagement.java:110-116）；本实现的表本来
    /// 就以 UUID 为键，所以只在判定明确失效时才丢弃，卸载期间愤怒自然保留。
    pub fn tick(&mut self, is_valid: &impl Fn(Uuid) -> bool) {
        self.anger_by_suspect
            .retain(|uuid, anger| *anger > DEFAULT_ANGER_DECREASE && is_valid(*uuid));
        for anger in self.anger_by_suspect.values_mut() {
            *anger -= DEFAULT_ANGER_DECREASE;
        }
        let remaining = &self.anger_by_suspect;
        self.suspects.retain(|s| remaining.contains_key(&s.uuid));
        for suspect in &mut self.suspects {
            if let Some(anger) = remaining.get(&suspect.uuid) {
                suspect.anger = *anger;
            }
        }
        self.sort_and_update_highest_anger();
    }

    /// 原版 `AngerManagement.sortAndUpdateHighestAnger`（AngerManagement.java:124-130）
    /// 加上 `Sorter.compare`（AngerManagement.java:178-197）的副作用。
    ///
    /// 原版的 `highestAnger` 是在 `Sorter.compare` 里被顺带刷新的，因此单元素列表
    /// 走不到比较器、需要额外补一次赋值（原版 `if (suspects.size() == 1)`）。
    /// 这里直接取全表最大值，语义等价且不依赖比较次数。
    fn sort_and_update_highest_anger(&mut self) {
        self.suspects.sort_by(|a, b| {
            // 已经愤怒的排前面。
            let a_angry = AngerLevel::by_anger(a.anger).is_angry();
            let b_angry = AngerLevel::by_anger(b.anger).is_angry();
            b_angry
                .cmp(&a_angry)
                // 其次玩家优先。
                .then_with(|| b.is_player.cmp(&a.is_player))
                // 最后按愤怒值降序。
                .then_with(|| b.anger.cmp(&a.anger))
        });
        self.highest_anger = self.suspects.iter().map(|s| s.anger).max().unwrap_or(0);
    }

    /// 原版 `AngerManagement.getActiveAnger`（AngerManagement.java:167-169）：
    /// 有当前目标就取该目标的愤怒值，否则取全表最高。
    #[must_use]
    pub fn active_anger(&self, current_target: Option<Uuid>) -> i32 {
        match current_target {
            Some(uuid) => self.anger_by_suspect.get(&uuid).copied().unwrap_or(0),
            None => self.highest_anger,
        }
    }

    /// 原版 `AngerManagement.getTopSuspect` + `getActiveEntity`
    /// （AngerManagement.java:163-173）：排序后第一个通过判定的嫌疑对象。
    #[must_use]
    pub fn top_suspect(&self, is_valid: &impl Fn(Uuid) -> bool) -> Option<Uuid> {
        self.suspects
            .iter()
            .map(|s| s.uuid)
            .find(|uuid| is_valid(*uuid))
    }

    /// 序列化用：导出全部 (UUID, 愤怒值) 对，对应原版
    /// `AngerManagement.createUuidAngerPairs`（AngerManagement.java:80-82）。
    pub fn suspect_pairs(&self) -> impl Iterator<Item = (Uuid, i32)> + '_ {
        self.anger_by_suspect
            .iter()
            .map(|(uuid, anger)| (*uuid, *anger))
    }

    /// 反序列化用：对应原版 `AngerManagement` 构造函数从 `angerByUuid`
    /// 恢复（AngerManagement.java:71-78）。此时无法得知对象是否玩家，
    /// 统一按非玩家载入；首次 `increase_anger` 会补上正确的标记。
    pub fn insert_saved(&mut self, uuid: Uuid, anger: i32) {
        let anger = anger.clamp(0, MAX_ANGER);
        self.anger_by_suspect.insert(uuid, anger);
        self.suspects.push(Suspect {
            uuid,
            anger,
            is_player: false,
        });
        self.sort_and_update_highest_anger();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anger_level_thresholds_match_vanilla() {
        assert_eq!(AngerLevel::by_anger(0), AngerLevel::Calm);
        assert_eq!(AngerLevel::by_anger(39), AngerLevel::Calm);
        assert_eq!(AngerLevel::by_anger(40), AngerLevel::Agitated);
        assert_eq!(AngerLevel::by_anger(79), AngerLevel::Agitated);
        assert_eq!(AngerLevel::by_anger(80), AngerLevel::Angry);
        assert!(AngerLevel::Angry.is_angry());
        assert!(!AngerLevel::Agitated.is_angry());
    }

    #[test]
    fn increase_anger_clamps_at_max() {
        let mut anger = AngerManagement::default();
        let uuid = Uuid::from_u128(1);
        assert_eq!(anger.increase_anger(uuid, true, 35), 35);
        assert_eq!(anger.increase_anger(uuid, true, 200), MAX_ANGER);
        assert_eq!(anger.active_anger(Some(uuid)), MAX_ANGER);
    }

    #[test]
    fn tick_decays_and_drops_exhausted_suspects() {
        let mut anger = AngerManagement::default();
        let uuid = Uuid::from_u128(2);
        anger.increase_anger(uuid, true, 3);
        let always_valid = |_: Uuid| true;
        anger.tick(&always_valid);
        assert_eq!(anger.active_anger(Some(uuid)), 2);
        anger.tick(&always_valid);
        // 降到 1 时原版直接移除，不再保留 0 值条目。
        assert_eq!(anger.active_anger(Some(uuid)), 0);
        assert!(anger.top_suspect(&always_valid).is_none());
    }

    #[test]
    fn tick_drops_invalid_suspects() {
        let mut anger = AngerManagement::default();
        let uuid = Uuid::from_u128(3);
        anger.increase_anger(uuid, true, 100);
        anger.tick(&|_| false);
        assert!(anger.top_suspect(&|_| true).is_none());
    }

    #[test]
    fn angry_suspect_sorts_before_calmer_player() {
        let mut anger = AngerManagement::default();
        let angry_mob = Uuid::from_u128(4);
        let calm_player = Uuid::from_u128(5);
        anger.increase_anger(calm_player, true, 10);
        anger.increase_anger(angry_mob, false, 90);
        assert_eq!(anger.top_suspect(&|_| true), Some(angry_mob));
        // 无目标时取全表最高愤怒值。
        assert_eq!(anger.active_anger(None), 90);
    }

    #[test]
    fn player_wins_tie_between_equally_calm_suspects() {
        let mut anger = AngerManagement::default();
        let mob = Uuid::from_u128(6);
        let player = Uuid::from_u128(7);
        anger.increase_anger(mob, false, 20);
        anger.increase_anger(player, true, 20);
        assert_eq!(anger.top_suspect(&|_| true), Some(player));
    }

    #[test]
    fn saved_anger_round_trips() {
        let mut anger = AngerManagement::default();
        anger.insert_saved(Uuid::from_u128(8), 55);
        let pairs: Vec<_> = anger.suspect_pairs().collect();
        assert_eq!(pairs, vec![(Uuid::from_u128(8), 55)]);
        assert_eq!(anger.active_anger(None), 55);
    }
}
