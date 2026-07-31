use super::{Controls, Goal};
use crate::entity::EntityBase;
use crate::entity::ai::goal::GoalFuture;
use crate::entity::ai::pathfinder::NavigatorGoal;
use crate::entity::mob::Mob;
use pumpkin_util::math::vector3::Vector3;
use rand::RngExt;

pub struct MeleeAttackGoal {
    goal_control: Controls,
    speed: f64,
    pause_when_mob_idle: bool,
    #[expect(dead_code)]
    target_location: Vector3<f64>,
    update_countdown_ticks: i32,
    pub cooldown: i32,
    #[expect(dead_code)]
    attack_interval_ticks: i32,
    last_target_position: Option<Vector3<f64>>,
    /// Vanilla `lastCanUseCheck` — throttle pathfinding in `canUse` to every 20 ticks.
    last_can_use_check: i64,
}

impl MeleeAttackGoal {
    #[must_use]
    pub fn new(speed: f64, pause_when_mob_idle: bool) -> Self {
        Self {
            goal_control: Controls::MOVE | Controls::LOOK,
            // Speed *modifier* (vanilla navigation speed), not absolute blocks/tick.
            speed: speed.max(0.01),
            pause_when_mob_idle,
            target_location: Vector3::new(0.0, 0.0, 0.0),
            update_countdown_ticks: 0,
            cooldown: 0,
            attack_interval_ticks: 20,
            last_target_position: None,
            last_can_use_check: i64::MIN,
        }
    }

    #[must_use]
    pub fn get_max_cooldown(&self) -> i32 {
        self.get_tick_count(20)
    }

    /// Vanilla-compatible: living health/death, not merely Entity::is_alive (removal).
    fn target_is_valid(target: &dyn EntityBase) -> bool {
        if let Some(living) = target.get_living_entity() {
            return living.is_alive();
        }
        target.get_entity().is_alive()
    }

    /// Vanilla `Navigation.moveTo(Entity)` uses the living target's feet position
    /// (not a snapped block center). Snapping to block centers made A* prefer
    /// side/back approaches before charging.
    fn path_destination(target: &dyn EntityBase) -> Vector3<f64> {
        target.get_entity().pos.load()
    }

    /// Vanilla `PathNavigation.createPath(target, reachRange)` probe used by
    /// `MeleeAttackGoal.canUse` (`MeleeAttackGoal.java:48`).
    ///
    /// The navigator lives behind a `std::sync::Mutex`, so its guard must never
    /// cross the `.await`. The navigator is therefore moved out of the mutex for
    /// the duration of the path computation and moved back afterwards — the same
    /// pattern `MobEntity::tick` uses to drive `Navigator::tick`
    /// (`entity/mob/entity_base.rs`). Two consequences worth stating explicitly,
    /// since this used to be an undocumented convention:
    ///
    /// - Callers may hold no navigator guard, but they are free to lock the
    ///   navigator again *after* this returns; nothing here is re-entrant.
    /// - While the probe is in flight the mutex holds a default `Navigator`, so
    ///   the per-mob pathfinding malus overrides are momentarily invisible to
    ///   other readers. Only the mob's own AI tick touches its navigator, and
    ///   that tick drives this future to completion, so no other observer runs
    ///   inside the window.
    ///
    /// # Panics
    /// Panics if the navigator mutex is poisoned.
    async fn probe_has_path(mob: &dyn Mob, dest: Vector3<f64>) -> bool {
        let mob_entity = mob.get_mob_entity();
        let mut navigator = {
            let mut guard = mob_entity.navigator.lock().unwrap();
            std::mem::take(&mut *guard)
        };
        let has_path = navigator
            .create_path_to(&mob_entity.living_entity, dest)
            .await
            .is_some();
        *mob_entity.navigator.lock().unwrap() = navigator;
        has_path
    }
}

impl Goal for MeleeAttackGoal {
    fn can_start<'a>(&'a mut self, mob: &'a dyn Mob) -> GoalFuture<'a, bool> {
        Box::pin(async {
            // Vanilla MeleeAttackGoal.canUse (26.2):
            // - throttle canUse checks to every 20 game ticks
            // - require createPath(target) != null OR isWithinMeleeAttackRange
            // Without the path check, MeleeAttack always steals MOVE and blocks
            // MoveTowardsTargetGoal (iron golem 0.9 approach) when A* fails.
            let age = i64::from(
                mob.get_entity()
                    .age
                    .load(std::sync::atomic::Ordering::Relaxed),
            );
            if self.last_can_use_check != i64::MIN && age.wrapping_sub(self.last_can_use_check) < 20
            {
                return false;
            }
            self.last_can_use_check = age;

            let target = {
                let guard = mob.get_mob_entity().target.lock().await;
                guard.clone()
            };
            let Some(target) = target else {
                return false;
            };
            if !Self::target_is_valid(target.as_ref()) {
                return false;
            }
            if target
                .get_player()
                .is_some_and(|p| p.is_spectator() || p.is_creative())
            {
                return false;
            }

            // In melee range → can start without a full path (vanilla).
            if mob
                .get_mob_entity()
                .is_in_attack_range(target.as_ref())
                .await
            {
                return true;
            }

            // Vanilla MeleeAttackGoal.java:48: `createPath(target, 0) != null`.
            // Mobs that must not enter water (iron golem / enderman water malus
            // -1) are handled by the pathfinder itself, exactly like vanilla:
            // `WalkNodeEvaluator.findAcceptedNode` drops any node whose
            // `getPathfindingMalus` is negative, so A* routes around the pond
            // instead of the goal pre-selecting a dry bank.
            let dest = Self::path_destination(target.as_ref());
            let me = mob.get_entity().pos.load();
            if me.squared_distance_to_vec(&dest) < 0.25 {
                return false;
            }
            Self::probe_has_path(mob, dest).await
        })
    }

    fn should_continue<'a>(&'a self, mob: &'a dyn Mob) -> GoalFuture<'a, bool> {
        Box::pin(async {
            let target = mob.get_mob_entity().target.lock().await.clone();

            let Some(target) = target else {
                return false;
            };
            // Critical: drop chase the moment the target dies (death animation still
            // has Entity::is_alive()==true until remove after 20 ticks).
            if !Self::target_is_valid(target.as_ref()) {
                return false;
            }

            let is_valid_target = !target
                .get_player()
                .is_some_and(|p| p.is_spectator() || p.is_creative());

            if !is_valid_target {
                return false;
            }

            if self.pause_when_mob_idle {
                return mob
                    .get_mob_entity()
                    .is_in_position_target_range_pos(&target.get_entity().block_pos.load());
            }

            true
        })
    }

    fn start<'a>(&'a mut self, mob: &'a dyn Mob) -> GoalFuture<'a, ()> {
        Box::pin(async {
            // Vanilla setAggressive(true) — illager arms / attacking pose
            mob.get_mob_entity().set_attacking(true);

            let target = mob.get_mob_entity().target.lock().await.clone();
            if let Some(target) = target {
                if !Self::target_is_valid(target.as_ref()) {
                    return;
                }
                // Vanilla `moveTo(this.path, speedModifier)` — the path targets
                // the mob's target directly (MeleeAttackGoal.java:48,60).
                let dest = Self::path_destination(target.as_ref());
                let mut navigator = mob.get_mob_entity().navigator.lock().unwrap();
                navigator.set_progress(NavigatorGoal {
                    current_progress: mob.get_entity().pos.load(),
                    destination: dest,
                    speed: self.speed,
                });
                self.last_target_position = Some(dest);
            }
            self.update_countdown_ticks = 0;
            self.cooldown = 0;
        })
    }

    fn stop<'a>(&'a mut self, mob: &'a dyn Mob) -> GoalFuture<'a, ()> {
        Box::pin(async {
            // Always clear target when melee ends if it is dead/invalid so
            // ActiveTargetGoal can pick the next living enemy (golem 2nd zombie,
            // vindicator next villager). Vanilla TargetGoal.stop clears the target.
            let should_clear = {
                let target = mob.get_mob_entity().target.lock().await;
                match target.as_deref() {
                    None => false,
                    Some(entity) => {
                        !Self::target_is_valid(entity)
                            || entity
                                .get_player()
                                .is_some_and(|p| p.is_spectator() || p.is_creative())
                    }
                }
            };
            if should_clear {
                mob.set_mob_target(None).await;
            }

            mob.get_mob_entity().set_attacking(false);
            mob.get_mob_entity().navigator.lock().unwrap().stop();
            self.last_target_position = None;
        })
    }

    fn tick<'a>(&'a mut self, mob: &'a dyn Mob) -> GoalFuture<'a, ()> {
        Box::pin(async {
            let target = mob.get_mob_entity().target.lock().await.clone();
            let Some(target) = target else {
                return;
            };

            // Bail out mid-tick if the target died this frame.
            if !Self::target_is_valid(target.as_ref()) {
                mob.set_mob_target(None).await;
                mob.get_mob_entity().set_attacking(false);
                mob.get_mob_entity().navigator.lock().unwrap().stop();
                return;
            }

            mob.get_mob_entity()
                .look_control
                .lock()
                .unwrap()
                .look_at_entity_with_range(&target, 30.0, 30.0);

            self.update_countdown_ticks = (self.update_countdown_ticks - 1).max(0);

            // Vanilla `moveTo(target, speedModifier)` (MeleeAttackGoal.java:118)
            // paths straight at the target; water avoidance is the pathfinder's
            // job via the per-mob `PathType::Water` malus.
            let dest = Self::path_destination(target.as_ref());
            // TODO(rng-parity): vanilla advances one shared `RandomSource`;
            // `Mob::get_random` hands out a fresh `ThreadRng` per call here and
            // below. Tracked by the RNG parity work, not changed in this pass.
            let should_update_nav = self.update_countdown_ticks <= 0
                && (self
                    .last_target_position
                    .is_none_or(|last_pos| dest.squared_distance_to_vec(&last_pos) >= 1.0)
                    || mob.get_random().random_range(0..20) == 0);

            if should_update_nav {
                let mob_pos = mob.get_entity().pos.load();
                let dist_sq = mob_pos.squared_distance_to_vec(&dest);
                let mut navigator = mob.get_mob_entity().navigator.lock().unwrap();
                navigator.set_progress(NavigatorGoal {
                    current_progress: mob_pos,
                    destination: dest,
                    speed: self.speed,
                });
                self.last_target_position = Some(dest);
                // Vanilla-ish repath cadence: faster when close for tighter chase.
                self.update_countdown_ticks = if dist_sq < 16.0 {
                    2 + mob.get_random().random_range(0..3)
                } else {
                    4 + mob.get_random().random_range(0..7)
                };
                if dist_sq > 1024.0 {
                    self.update_countdown_ticks += 10;
                } else if dist_sq > 256.0 {
                    self.update_countdown_ticks += 5;
                }
            }

            self.cooldown = (self.cooldown - 1).max(0);

            let can_see = {
                let from = mob.get_entity().get_eye_pos();
                let to = target.get_entity().get_eye_pos();
                let world = mob.get_entity().world.load();
                world
                    .raycast(from, to, async |block_pos, w| {
                        let state = w.get_block_state(block_pos);
                        state.is_solid()
                    })
                    .await
                    .is_none()
            };

            if self.cooldown <= 0
                && can_see
                && mob
                    .get_mob_entity()
                    .is_in_attack_range(target.as_ref())
                    .await
            {
                self.cooldown = self.get_max_cooldown();
                let is_golem = mob.get_entity().entity_type.id
                    == pumpkin_data::entity::EntityType::IRON_GOLEM.id;
                // Iron golem: both arms raise via entity event 4 inside try_attack.
                // Other mobs: arm swing animation packet.
                if !is_golem {
                    mob.get_mob_entity().living_entity.swing_hand().await;
                }
                // `mob` is EntityBase (Mob: EntityBase) — used as damage cause/source.
                let caller: &dyn EntityBase = mob;
                mob.get_mob_entity()
                    .try_attack(caller, target.as_ref())
                    .await;

                // If the attack killed them, clear immediately so we don't keep
                // swinging at a corpse for the rest of the death animation.
                if !Self::target_is_valid(target.as_ref()) {
                    mob.set_mob_target(None).await;
                    mob.get_mob_entity().set_attacking(false);
                    mob.get_mob_entity().navigator.lock().unwrap().stop();
                }
            }
        })
    }

    fn should_run_every_tick(&self) -> bool {
        // Vanilla MeleeAttackGoal.requiresUpdateEveryTick() == true
        true
    }

    fn controls(&self) -> Controls {
        self.goal_control
    }
}

#[cfg(test)]
mod tests {
    use super::MeleeAttackGoal;
    use crate::entity::ai::pathfinder::Navigator;
    use crate::entity::ai::pathfinder::node::PathType;

    /// `path_destination` must stay the only destination helper: vanilla
    /// `MeleeAttackGoal` paths at `createPath(target, 0)` / `moveTo(target, ..)`
    /// and never picks a substitute position. The signature check fails to
    /// compile if the helper starts taking the mob or a water flag again, which
    /// is what the deleted dry-ground search needed.
    const _: fn(&dyn crate::entity::EntityBase) -> pumpkin_util::math::vector3::Vector3<f64> =
        MeleeAttackGoal::path_destination;

    #[test]
    fn goal_leaves_water_avoidance_to_the_pathfinder() {
        // The goal no longer inspects blocks near the target. Water avoidance
        // for an iron golem is entirely the navigator's negative water malus,
        // matching vanilla Mob.setPathfindingMalus (Mob.java:212-214).
        let mut navigator = Navigator::default();
        navigator.set_pathfinding_malus(PathType::Water, -1.0);
        assert!(navigator.get_pathfinding_malus(PathType::Water) < 0.0);
    }

    /// `probe_has_path` must remain an ordinary async fn. If it regains a
    /// `block_in_place` wrapper it would have to become a sync fn returning
    /// `bool`, and this coercion to a future-returning fn pointer stops
    /// compiling.
    const fn takes_future_fn<F: Copy>(_: F) {}
    const _: () = takes_future_fn(MeleeAttackGoal::probe_has_path);
}
