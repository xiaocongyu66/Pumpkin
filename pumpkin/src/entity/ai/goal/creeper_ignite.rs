use std::sync::Weak;
use std::sync::atomic::Ordering;

use super::{Controls, Goal};
use crate::entity::EntityBase;
use crate::entity::ai::goal::GoalFuture;
use crate::entity::mob::Mob;
use crate::entity::mob::creeper::CreeperEntity;

pub struct CreeperIgniteGoal {
    goal_control: Controls,
    /// Weak: the creeper owns this goal via its goal selector, so a strong handle
    /// here would be a reference cycle that leaks the entity.
    creeper: Weak<CreeperEntity>,
}

impl CreeperIgniteGoal {
    #[must_use]
    pub const fn new(creeper: Weak<CreeperEntity>) -> Self {
        Self {
            goal_control: Controls::MOVE,
            creeper,
        }
    }
}

impl Goal for CreeperIgniteGoal {
    fn can_start<'a>(&'a mut self, mob: &'a dyn Mob) -> GoalFuture<'a, bool> {
        Box::pin(async move {
            let Some(self_creeper) = self.creeper.upgrade() else {
                return false;
            };
            let creeper = mob.get_mob_entity();
            let target_lock = creeper.target.lock().await;

            if self_creeper.fuse_speed.load(Ordering::Relaxed) > 0 {
                return true;
            }

            if let Some(target) = target_lock.as_ref() {
                let dist_sq = mob
                    .get_entity()
                    .pos
                    .load()
                    .squared_distance_to_vec(&target.get_entity().pos.load());
                return dist_sq < 9.0;
            }

            false
        })
    }

    fn start<'a>(&'a mut self, mob: &'a dyn Mob) -> GoalFuture<'a, ()> {
        Box::pin(async move {
            let mut navigator = mob.get_mob_entity().navigator.lock().unwrap();
            navigator.stop();
        })
    }

    fn stop<'a>(&'a mut self, _mob: &'a dyn Mob) -> GoalFuture<'a, ()> {
        Box::pin(async move {
            if let Some(creeper) = self.creeper.upgrade() {
                creeper.set_fuse_speed(-1);
            }
        })
    }

    fn tick<'a>(&'a mut self, mob: &'a dyn Mob) -> GoalFuture<'a, ()> {
        Box::pin(async move {
            let Some(self_creeper) = self.creeper.upgrade() else {
                return;
            };
            let target_lock = mob.get_mob_entity().target.lock().await;

            let Some(target) = target_lock.as_ref() else {
                self_creeper.set_fuse_speed(-1);
                return;
            };

            let dist_sq = mob
                .get_entity()
                .pos
                .load()
                .squared_distance_to_vec(&target.get_entity().pos.load());

            if dist_sq > 49.0 {
                self_creeper.set_fuse_speed(-1);
            } else {
                // Only charge fuse when target is visible (vanilla).
                let from = self_creeper.get_entity().get_eye_pos();
                let to = target.get_entity().get_eye_pos();
                let world = self_creeper.get_entity().world.load();
                let can_see = world
                    .raycast(from, to, async |block_pos, w| {
                        let state = w.get_block_state(block_pos);
                        state.is_solid()
                    })
                    .await
                    .is_none();
                self_creeper.set_fuse_speed(if can_see { 1 } else { -1 });
            }
        })
    }

    fn should_run_every_tick(&self) -> bool {
        true
    }

    fn controls(&self) -> Controls {
        self.goal_control
    }
}
