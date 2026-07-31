//! 村民对 POI 票据的认领与归还。
//!
//! # 与原版的对应
//!
//! 原版村民用 Brain 的记忆槽记录认领结果，取票在 `AcquirePoi`
//! （`/root/Vanilla/src/net/minecraft/world/entity/ai/behavior/AcquirePoi.java:107-115`）：
//! 先按 `Occupancy.HAS_SPACE` 过滤、距离近的优先，再由
//! `PoiManager.take`（`PoiManager.java:134-139`）调 `PoiRecord.acquireTicket`
//! 并把位置写进记忆槽。
//!
//! 还票在 `Villager.releasePoi`（`Villager.java:577-592`）：只有当那个位置**仍然**
//! 注册着**类型匹配**的记录时才 `PoiManager.release`。`Villager.releaseAllPois`
//! （`Villager.java:557-561`）在死亡（`Villager.java:553`）和被闪电转化成女巫
//! （`Villager.java:700`）时把四个记忆槽的票全部还回去。
//!
//! Pumpkin 没有 Brain，所以记忆槽映射到村民实体上的字段：
//!
//! | 原版记忆槽 | Pumpkin 字段 | POI 类型判定 |
//! |---|---|---|
//! | `MemoryModuleType.HOME` | `VillagerEntity::home_pos` | `poiType.is(PoiTypes.HOME)` |
//! | `MemoryModuleType.JOB_SITE` | `VillagerEntity::job_site` | `#minecraft:acquirable_job_site` |
//!
//! `MEETING_POINT`（钟）和 `POTENTIAL_JOB_SITE`（走向工作站的中间态）Pumpkin 还
//! 没有对应字段，所以这里不涉及。
//!
//! # 为什么还要额外记「我到底持不持有这张票」
//!
//! 原版不需要这个标记：记忆槽和票据在 `AcquirePoi` 里同时写入，又一起随存档落盘
//! （记忆槽在实体 NBT，票据在 `poi/` region 文件），所以「记忆槽有值」永远等价于
//! 「我手里有票」。
//!
//! Pumpkin 的两半也各自持久化（`home_pos` 见 `villager/nbt.rs:41-46`，
//! `free_tickets` 见 `PoiEntry`），新存档同样成立。但**老存档**不成立：票据认领是
//! 现在才接上的，此前的存档里村民早已带着 `home_pos`，而 POI 记录的票据还是满的。
//! 若只信 `home_pos`，这些村民永远不会走认领分支，记录也就永远不是 `IS_OCCUPIED`，
//! 村庄判定会继续失效。
//!
//! 因此这里用两个**不持久化**的标记记住本次在世期间的取票结果，并在每次校验时对账
//! （[`VillagerEntity::ensure_home_ticket`]）：手里没票就补取一次，补不到说明这个
//! POI 已被别人占了，于是放弃绑定。副作用是归还只发生在确实持票时，不会把别的村民
//! 的票误还回去。
//!
//! # 为什么票据是「两个村民不会抢同一张床」的机制
//!
//! `minecraft:home` 与 13 种工作站的 `maxTickets` 都是 1（`PoiTypes.java:91-104`），
//! 取走唯一那张票之后 `PoiRecord.hasSpace` 就为假，后来的村民取不到票。所以认领
//! 归属完全由存储决出，不需要再去扫周围村民已认领的位置。
//!
//! # 锁
//!
//! `World::portal_poi` 是不可重入的 `tokio::sync::Mutex`，而
//! `World::update_poi_on_block_state_change` 会在 `set_block_state` 末尾锁它
//! （`world/poi.rs:52`）。因此这里的每个函数都只在自己内部短暂持锁，绝不把锁
//! 跨到会改方块的调用上去。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pumpkin_util::math::position::BlockPos;
use pumpkin_world::poi::{PoiType, types};

use crate::entity::EntityBase;
use crate::world::World;

use super::VillagerEntity;

/// 原版 `p -> p.is(PoiTypes.HOME)`（`VillagerGoalPackages.java:99`，
/// `Villager.POI_MEMORIES` 里 `HOME` 那一项，`Villager.java:168`）。
fn is_home_poi(poi_type: &PoiType) -> bool {
    poi_type.name == types::HOME.name
}

/// 原版 `VillagerProfession.ALL_ACQUIRABLE_JOBS`，即 `#minecraft:acquirable_job_site`
/// tag（`Villager.java:168` 里 `POTENTIAL_JOB_SITE` 那一项）。
///
/// 用 tag 而不是「当前职业对应的那一种」，对应原版取票时用的
/// `profession.acquirableJobSite()`；调用方在收集候选时已经按职业筛过方块了。
fn is_acquirable_job_site_poi(poi_type: &PoiType) -> bool {
    poi_type.acquirable_job_site
}

/// 本次在世期间「这张票在我手上」的标记，见模块文档。
///
/// 不写 NBT：票据本身已经随 `poi/` region 文件持久化，重启后由
/// [`VillagerEntity::ensure_home_ticket`] / [`VillagerEntity::ensure_job_site_ticket`]
/// 重新对账取回，不需要也不应该再存一份。
#[derive(Debug, Default)]
pub struct HeldTickets {
    home: AtomicBool,
    job_site: AtomicBool,
}

impl HeldTickets {
    fn home(&self) -> bool {
        self.home.load(Ordering::Relaxed)
    }

    fn set_home(&self, held: bool) {
        self.home.store(held, Ordering::Relaxed);
    }

    fn job_site(&self) -> bool {
        self.job_site.load(Ordering::Relaxed)
    }

    fn set_job_site(&self, held: bool) {
        self.job_site.store(held, Ordering::Relaxed);
    }
}

impl VillagerEntity {
    /// 原版 `PoiManager.take`（`PoiManager.java:134-139`）：在候选里找第一个
    /// 类型匹配且还有空位的记录，取走它一张票并返回位置。
    ///
    /// 候选必须由调用方按距离**从近到远**排好，这样第一个取票成功的就是原版
    /// `findAllClosestFirstWithType`（`PoiManager.java:113-115`）挑中的那个。
    ///
    /// `exists` 先卡一道类型判定，对应原版 `take` 的 `predicate`；位置上没有记录
    /// （方块还没被索引，或索引已经摘掉）时取票失败，村民就不会认领一个存储里
    /// 并不存在的 POI。
    async fn take_poi_ticket(
        world: &Arc<World>,
        candidates: &[BlockPos],
        matches: fn(&PoiType) -> bool,
    ) -> Option<BlockPos> {
        let mut storage = world.portal_poi.lock().await;
        let taken = candidates
            .iter()
            .copied()
            .find(|pos| storage.exists(pos, matches) && storage.acquire(pos));
        drop(storage);
        taken
    }

    /// 认领床位：对应原版
    /// `AcquirePoi.create(p -> p.is(PoiTypes.HOME), MemoryModuleType.HOME, ..)`
    /// （`VillagerGoalPackages.java:99`）。
    ///
    /// 取到票才写 `home_pos`，两者同时成立；取不到票（候选全被别的村民占了）就保持
    /// 未认领状态，下一轮再试，和原版 `AcquirePoi` 找不到 `HAS_SPACE` 的记录时不写
    /// 记忆槽一致。
    pub(super) async fn acquire_home(&self, candidates: &[BlockPos]) -> bool {
        let world = self.get_entity().world.load();
        let Some(pos) = Self::take_poi_ticket(&world, candidates, is_home_poi).await else {
            return false;
        };
        *self.home_pos.lock().unwrap() = Some(pos);
        self.held_tickets.set_home(true);
        true
    }

    /// 认领工作站：对应原版
    /// `AcquirePoi.create(profession.acquirableJobSite(), MemoryModuleType.JOB_SITE, ..)`
    /// （`VillagerGoalPackages.java:99`）。
    pub(super) async fn acquire_job_site(&self, candidates: &[BlockPos]) -> Option<BlockPos> {
        let world = self.get_entity().world.load();
        let pos = Self::take_poi_ticket(&world, candidates, is_acquirable_job_site_poi).await?;
        *self.job_site.lock().unwrap() = Some(pos);
        self.held_tickets.set_job_site(true);
        Some(pos)
    }

    /// 补取一张「本该已经在我手上」的票，让记录重新变成 `IS_OCCUPIED`。
    ///
    /// 记录已经没有空位就什么都不做 —— 那正是正常情形（票据随 `poi/` region 文件
    /// 一起持久化，重启后本来就还是被取走的状态）。真正需要补取的是两种记录票据被
    /// 重置回满的情况：老存档（票据认领是现在才接上的），以及区块重扫新建了记录。
    ///
    /// 幂等：`PoiRecord.acquireTicket` 只在 `free_tickets > 0` 时才递减
    /// （`PoiRecord.java:51-58`），所以重复调用不会把票据扣穿。
    async fn top_up_poi_ticket(world: &Arc<World>, pos: BlockPos, matches: fn(&PoiType) -> bool) {
        let mut storage = world.portal_poi.lock().await;
        if storage.exists(&pos, matches) {
            let _ = storage.acquire(&pos);
        }
        drop(storage);
    }

    /// 对账 `home_pos` 与存储里的票据。返回绑定是否仍然成立。
    ///
    /// 原版没有这一步：记忆槽和票据在它那里从不分叉，理由见模块文档。这里分两种
    /// 情况：
    ///
    /// - 票在我手上（本次取到的，或从 NBT 恢复的）：补取一次以覆盖老存档和区块重扫
    ///   把票据重置回满的情况，绑定成立。
    /// - 票不在我手上：这是刚认领失败后又被外部写了 `home_pos` 之类的边缘情形，
    ///   照常取票；取不到说明床已被别人占了，绑定不成立。
    pub(super) async fn ensure_home_ticket(&self, pos: BlockPos) -> bool {
        if self.held_tickets.home() {
            let world = self.get_entity().world.load();
            Self::top_up_poi_ticket(&world, pos, is_home_poi).await;
            return true;
        }
        self.acquire_home(&[pos]).await
    }

    /// [`Self::ensure_home_ticket`] 的工作站版本。
    pub(super) async fn ensure_job_site_ticket(&self, pos: BlockPos) -> bool {
        if self.held_tickets.job_site() {
            let world = self.get_entity().world.load();
            Self::top_up_poi_ticket(&world, pos, is_acquirable_job_site_poi).await;
            return true;
        }
        self.acquire_job_site(&[pos]).await.is_some()
    }

    /// 从 NBT 恢复「票据在我手上」的标记。
    ///
    /// 原版的不变量是「记忆槽有值 ⇔ 票在我手上」（票据随 `poi/` region 文件落盘，
    /// 记忆槽随实体 NBT 落盘，两者一起恢复）。所以读回 `home_pos` / `job_site` 就等
    /// 于读回了持票状态，不能当成「未持票」——否则重新加载后会去抢自己上一次已经
    /// 取走的那张票，抢不到就放弃绑定，而那张票再也没人归还（永久泄漏）。
    pub(super) fn restore_held_tickets(&self, home: bool, job_site: bool) {
        self.held_tickets.set_home(home);
        self.held_tickets.set_job_site(job_site);
    }

    /// 原版 `Villager.releasePoi`（`Villager.java:577-592`）。
    ///
    /// 只有当 `pos` 上仍注册着类型匹配的记录时才还票。记录已经不在了（方块被破坏，
    /// `world/poi.rs:54-56` 把它摘掉了）就什么都不做 —— 票据随记录一起消失，
    /// 和原版 `ValidateNearbyPoi` 在 `!exists` 时只擦记忆、不 `release` 一致
    /// （`ValidateNearbyPoi.java:41-42`）。
    async fn yield_poi_ticket(world: &Arc<World>, pos: BlockPos, matches: fn(&PoiType) -> bool) {
        let mut storage = world.portal_poi.lock().await;
        if storage.exists(&pos, matches) {
            // `exists` 已经确认这里有记录，所以 `release` 不会返回 `None`
            //（原版无记录时直接抛异常，`PoiManager.java:146-148`）。
            let _ = storage.release(&pos);
        }
        drop(storage);
    }

    /// 放弃床位：清掉 `home_pos` 并归还票据。
    ///
    /// 合起来对应原版「擦掉 `MemoryModuleType.HOME` + `releasePoi(HOME)`」。两步做
    /// 成一个函数，是为了让「字段有值」和「票据被占」这两件事不会分叉。
    pub(super) async fn clear_home(&self) {
        // 先把值取出来再 await：`home_pos` 是 `std::sync::Mutex`，守卫不能跨 await。
        let previous = self.home_pos.lock().unwrap().take();
        // 只有票确实在我手上才还，否则会把别的村民的票误还回去。
        let held = self.held_tickets.home();
        self.held_tickets.set_home(false);
        if held && let Some(pos) = previous {
            let world = self.get_entity().world.load();
            Self::yield_poi_ticket(&world, pos, is_home_poi).await;
        }
    }

    /// 放弃工作站：清掉 `job_site` 并归还票据。
    ///
    /// 对应原版「擦掉 `MemoryModuleType.JOB_SITE` + `releasePoi(JOB_SITE)`」，也是
    /// `YieldJobSite` 把工作站让给失业村民时走的那一步（`YieldJobSite.java:56-58`）。
    pub(super) async fn clear_job_site(&self) {
        let previous = self.job_site.lock().unwrap().take();
        let held = self.held_tickets.job_site();
        self.held_tickets.set_job_site(false);
        if held && let Some(pos) = previous {
            let world = self.get_entity().world.load();
            Self::yield_poi_ticket(&world, pos, is_acquirable_job_site_poi).await;
        }
    }

    /// 原版 `Villager.releaseAllPois`（`Villager.java:557-561`）。
    ///
    /// 村民离场（死亡、被丢弃、转化成僵尸村民或女巫）时必须走一遍，否则它占着的
    /// 床位和工作站会永久停在 `IS_OCCUPIED`，别的村民再也认领不到。
    pub async fn release_all_pois(&self) {
        self.clear_home().await;
        self.clear_job_site().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_predicate_only_accepts_the_home_type() {
        assert!(is_home_poi(&types::HOME));
        assert!(!is_home_poi(&types::MEETING));
        assert!(!is_home_poi(&types::FARMER));
        assert!(!is_home_poi(&types::NETHER_PORTAL));
    }

    #[test]
    fn job_site_predicate_follows_the_acquirable_job_site_tag() {
        assert!(is_acquirable_job_site_poi(&types::FARMER));
        assert!(is_acquirable_job_site_poi(&types::WEAPONSMITH));
        // 床和钟在 `#minecraft:village` 里，但不在 `#minecraft:acquirable_job_site`。
        assert!(!is_acquirable_job_site_poi(&types::HOME));
        assert!(!is_acquirable_job_site_poi(&types::MEETING));
        assert!(!is_acquirable_job_site_poi(&types::BEEHIVE));
    }

    #[test]
    fn claimable_poi_types_all_have_a_single_ticket() {
        // `PoiTypes.java:91-104`：这正是「两个村民不会认领同一张床/工作站」的来源。
        assert_eq!(types::HOME.max_tickets, 1);
        for poi_type in types::ALL {
            if poi_type.acquirable_job_site {
                assert_eq!(poi_type.max_tickets, 1, "{}", poi_type.name);
            }
        }
    }

    #[test]
    fn every_village_poi_type_can_actually_be_occupied() {
        // `IS_OCCUPIED` 靠票据被取走来成立，所以带 `#minecraft:village` 的类型必须
        // 有票可取，否则村庄判定永远看不到它。
        for poi_type in types::ALL {
            if poi_type.village {
                assert!(poi_type.max_tickets > 0, "{}", poi_type.name);
            }
        }
    }

    #[test]
    fn held_ticket_flags_start_clear_and_track_each_poi_independently() {
        let held = HeldTickets::default();
        assert!(!held.home());
        assert!(!held.job_site());

        held.set_home(true);
        assert!(held.home());
        // 床位和工作站是两张互不相干的票。
        assert!(!held.job_site());

        held.set_job_site(true);
        held.set_home(false);
        assert!(!held.home());
        assert!(held.job_site());
    }

    #[test]
    fn a_single_ticket_is_exclusive_and_returns_on_release() {
        use pumpkin_world::poi::PoiEntry;

        // 这是床位归属的全部机制：`minecraft:home` 只有一张票。
        let mut bed = PoiEntry::new(BlockPos::new(0, 64, 0), types::HOME.name);
        assert!(!bed.is_occupied());
        assert!(bed.has_space());

        // 第一个村民取到票，记录随即变成 `IS_OCCUPIED`。
        assert!(bed.acquire_ticket());
        assert!(bed.is_occupied());
        // 第二个村民抢不到，所以两个村民不会认领同一张床。
        assert!(!bed.acquire_ticket());
        assert!(!bed.has_space());

        // 村民离场归还后，床重新可被认领 —— 票据生命周期闭合。
        assert!(bed.release_ticket());
        assert!(bed.has_space());
        assert!(!bed.is_occupied());
        // 多余的归还不会把票据涨穿（`PoiRecord.releaseTicket` 的上限判定）。
        assert!(!bed.release_ticket());
    }
}
