//! Vanilla `LeapAtTargetGoal` — used by spiders (yd = 0.4).

use super::{Controls, Goal, GoalFuture};
use crate::entity::mob::Mob;
use pumpkin_util::math::vector3::Vector3;
use rand::RngExt;

pub struct LeapAtTargetGoal {
    /// Vertical impulse (vanilla spider: 0.4).
    yd: f64,
}

/// Vanilla `LeapAtTargetGoal.canUse` rejects `d < 4.0 || d > 16.0` on the
/// **squared** distance (LeapAtTargetGoal.java:33-36), so both ends are included.
const MIN_LEAP_DISTANCE_SQ: f64 = 4.0;
const MAX_LEAP_DISTANCE_SQ: f64 = 16.0;
/// Vanilla `LeapAtTargetGoal.start` horizontal scale (LeapAtTargetGoal.java:53).
const LEAP_SCALE: f64 = 0.4;
/// Vanilla weight kept from the existing delta movement (LeapAtTargetGoal.java:53).
const LEAP_INERTIA: f64 = 0.2;
/// Vanilla `delta.lengthSqr() > 1.0E-7` guard (LeapAtTargetGoal.java:52).
const LEAP_MIN_LENGTH_SQ: f64 = 1.0e-7;

impl LeapAtTargetGoal {
    #[must_use]
    pub fn new(yd: f64) -> Self {
        Self { yd }
    }

    /// Vanilla accepts the closed squared interval `[4.0, 16.0]`.
    #[must_use]
    pub fn is_in_leap_range(distance_sq: f64) -> bool {
        (MIN_LEAP_DISTANCE_SQ..=MAX_LEAP_DISTANCE_SQ).contains(&distance_sq)
    }

    /// Vanilla `LeapAtTargetGoal.start` (LeapAtTargetGoal.java:49-56):
    /// the horizontal delta towards the target is normalized, scaled by 0.4 and
    /// added to `getDeltaMovement().scale(0.2)`; the result **replaces** the
    /// velocity (`setDeltaMovement(delta.x, yd, delta.z)`), it is not accumulated.
    /// Note that the y component of the current movement is discarded because
    /// the delta vector is built with `y = 0.0`.
    #[must_use]
    pub fn leap_velocity(
        mob_pos: Vector3<f64>,
        target_pos: Vector3<f64>,
        velocity: Vector3<f64>,
        yd: f64,
    ) -> Vector3<f64> {
        let mut delta = Vector3::new(target_pos.x - mob_pos.x, 0.0, target_pos.z - mob_pos.z);
        if delta.length_squared() > LEAP_MIN_LENGTH_SQ {
            delta = delta.normalize() * LEAP_SCALE + velocity * LEAP_INERTIA;
        }
        Vector3::new(delta.x, yd, delta.z)
    }
}

impl Goal for LeapAtTargetGoal {
    fn can_start<'a>(&'a mut self, mob: &'a dyn Mob) -> GoalFuture<'a, bool> {
        Box::pin(async move {
            let target = mob.get_mob_entity().target.lock().await;
            let Some(target) = target.as_ref() else {
                return false;
            };
            let entity = mob.get_entity();
            // Must be on ground (vanilla checks onGround).
            if !entity.on_ground.load(std::sync::atomic::Ordering::Relaxed) {
                return false;
            }
            let mob_pos = entity.pos.load();
            let target_pos = target.get_entity().pos.load();
            let dist_sq = mob_pos.squared_distance_to_vec(&target_pos);
            // Vanilla `LeapAtTargetGoal.canUse` (LeapAtTargetGoal.java:33-36)
            // compares `distanceToSqr`, bailing on `d < 4.0 || d > 16.0` — an
            // inclusive squared window, i.e. 2..=4 blocks of real distance.
            // Taking the square root here made wolves and spiders refuse to
            // pounce inside biting range and leap from 16 blocks away instead.
            Self::is_in_leap_range(dist_sq) && mob.get_random().random_range(0..5) == 0
        })
    }

    fn should_continue<'a>(&'a self, mob: &'a dyn Mob) -> GoalFuture<'a, bool> {
        Box::pin(async move {
            !mob.get_entity()
                .on_ground
                .load(std::sync::atomic::Ordering::Relaxed)
        })
    }

    fn start<'a>(&'a mut self, mob: &'a dyn Mob) -> GoalFuture<'a, ()> {
        Box::pin(async move {
            let target = mob.get_mob_entity().target.lock().await;
            let Some(target) = target.as_ref() else {
                return;
            };
            let entity = mob.get_entity();
            let velocity = Self::leap_velocity(
                entity.pos.load(),
                target.get_entity().pos.load(),
                entity.velocity.load(),
                self.yd,
            );
            entity.velocity.store(velocity);
        })
    }

    fn controls(&self) -> Controls {
        // Vanilla LeapAtTargetGoal.java:21 — `EnumSet.of(Flag.JUMP, Flag.MOVE)`.
        // Without MOVE the leap ran alongside MeleeAttackGoal and both fought
        // over the mob's movement.
        Controls::JUMP | Controls::MOVE
    }
}
