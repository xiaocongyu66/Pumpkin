//! `LivingEntity` 的 `EntityBase` 实现：tick、虚空伤害、重力与访问器。
//!
//! 受伤主流程在 [`super::damage`] 里，这里只做薄委托。对应原版
//! `LivingEntity.tick` / `aiStep`
//! (`/root/Vanilla/src/net/minecraft/world/entity/LivingEntity.java`)。

use super::LivingEntity;
use crate::entity::mob::slime::SlimeEntity;
use crate::entity::{Entity, EntityBase, EntityBaseFuture, NBTStorage};
use crate::server::Server;
use pumpkin_data::attributes::Attributes;
use pumpkin_data::damage::DamageType;
use pumpkin_data::data_component_impl::FoodImpl;
use pumpkin_data::entity::{EntityStatus, EntityType};
use pumpkin_data::item::Item;
use pumpkin_data::item_stack::ItemStack;
use pumpkin_data::meta_data_type::MetaDataType;
use pumpkin_data::sound::Sound;
use pumpkin_data::tracked_data::TrackedData;
use pumpkin_protocol::java::client::play::Metadata;
use pumpkin_util::GameMode;
use pumpkin_util::Hand;
use pumpkin_util::math::vector3::Vector3;
use std::sync::Arc;
use std::sync::atomic::Ordering::{self, Relaxed, SeqCst};

impl LivingEntity {
    pub const fn entity_id(&self) -> i32 {
        self.entity.entity_id
    }

    pub fn can_take_damage(&self) -> bool {
        !self.entity.invulnerable.load(Ordering::Relaxed) && self.is_part_of_game()
    }

    /// Vanilla `LivingEntity.isAlive` — health > 0 and not in death state.
    ///
    /// Note: `Entity::is_alive` only checks removal. Corpses stay in the world for
    /// ~20 ticks of death animation; targeting must use this method or they keep
    /// attacking dead villagers/zombies.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.health.load() > 0.0 && !self.dead.load(Ordering::Relaxed) && !self.entity.is_removed()
    }

    pub fn is_part_of_game(&self) -> bool {
        // Vanilla: !isSpectator() && isAlive()
        !self.is_spectator() && self.is_alive()
    }

    pub async fn reset_state(&self) {
        self.entity.reset_state().await;

        // Restore to maximum health for this entity type
        let max_health = self.get_max_health();
        self.set_health(max_health);
        // Clear any absorption
        self.absorption.store(0.0);
        // Send health metadata
        self.entity.send_meta_data(
            &[Metadata::new(
                TrackedData::HEALTH_ID,
                MetaDataType::FLOAT,
                max_health,
            )],
            None,
        );

        self.reset_effects_and_attributes().await;

        // Give a short grace period of invulnerability after respawn
        self.hurt_cooldown.store(20, Relaxed);
        self.last_damage_taken.store(0f32);

        self.entity.portal_cooldown.store(0, Relaxed);
        *self.entity.portal_manager.lock().await = None;

        // Clear fall/fire state
        self.fall_distance.store(0f32);
        self.death_time.store(0, Relaxed);
        self.entity.extinguish();
        self.entity.fire_ticks.store(0, Relaxed);

        // Clear velocity and movement input to remove persisted momentum
        self.entity.velocity.store(Vector3::default());
        self.entity.velocity_dirty.store(true, SeqCst);
        self.movement_input.store(Vector3::default());
        self.jumping.store(false, Relaxed);

        // If this LivingEntity corresponds to a Player, reset their hunger manager
        let world = self.entity.world.load();
        if let Some(player) = world.get_player_by_id(self.entity.entity_id) {
            player.hunger_manager.restart();
        }

        self.dead.store(false, Relaxed);
    }

    pub fn is_player(&self) -> bool {
        let world = self.entity.world.load();
        world.get_player_by_id(self.entity.entity_id).is_some()
    }

    pub fn get_movement(&self) -> Vector3<f64> {
        self.entity.movement.load()
    }

    pub(super) fn hurt_sound(&self) -> Sound {
        if self.entity.entity_type == &EntityType::SLIME {
            SlimeEntity::hurt_sound_for_size(self.entity.data.load(Relaxed))
        } else {
            Self::hurt_sound_for_entity(self.entity.entity_type)
        }
    }
}

impl EntityBase for LivingEntity {
    /// 薄委托：受伤主流程在 [`super::damage`]。
    fn damage_with_context<'a>(
        &'a self,
        caller: &'a dyn EntityBase,
        amount: f32,
        damage_type: DamageType,
        position: Option<Vector3<f64>>,
        source: Option<&'a dyn EntityBase>,
        cause: Option<&'a dyn EntityBase>,
    ) -> EntityBaseFuture<'a, bool> {
        self.damage_with_context_impl(caller, amount, damage_type, position, source, cause)
    }

    fn tick_in_void<'a>(&'a self, dyn_self: &'a dyn EntityBase) -> EntityBaseFuture<'a, ()> {
        Box::pin(async move {
            dyn_self
                .damage(dyn_self, 4.0, DamageType::OUT_OF_WORLD)
                .await;
        })
    }

    fn get_gravity(&self) -> f64 {
        self.get_attribute_value(&Attributes::GRAVITY)
    }

    #[allow(clippy::too_many_lines)]
    fn tick<'a>(
        &'a self,
        caller: &'a Arc<dyn EntityBase>,
        server: &'a Server,
    ) -> EntityBaseFuture<'a, ()> {
        Box::pin(async move {
            self.entity.tick(caller, server).await;

            // Only tick movement if the entity is alive. This prevents a dead "corpse"
            // from continuing to be simulated (accumulating fall_distance/velocity).
            // We allow movement during death animation (20 ticks) so knockback is applied.
            let is_alive = !self.dead.load(Relaxed) && self.health.load() > 0.0;
            let in_death_animation =
                self.health.load() <= 0.0 && self.death_time.load(Relaxed) < 20;
            if is_alive || (in_death_animation && self.entity.entity_type != &EntityType::PLAYER) {
                self.tick_movement(server, caller).await;
                // Vanilla-like order: freeze logic runs after movement/collisions.
                self.entity.tick_frozen(caller.as_ref()).await;
                // 原版 LivingEntity#aiStep 在冻结逻辑之后紧跟着 profiler "push" 段并调用
                // pushEntities（LivingEntity.java:2949），顺序是「移动/碰撞 → 冻结 → 推挤」。
                // 走 caller 而不是 self，保证矿车等重写过 push_entities 的实体能派发到自己的实现。
                caller.push_entities(caller).await;
            }

            // TODO
            let player = caller.get_player();
            let is_player = player.is_some();

            if !is_player {
                self.entity.send_pos_rot();
            }

            // Fetch supporting blocks for players or other entities
            let supporting_pos = caller.get_player().map_or_else(
                || self.entity.get_supporting_block_pos(),
                crate::entity::player::Player::get_supporting_block_pos,
            );

            // Notify the block under the entity each tick if a supporting block position is found
            if let Some(supporting) = supporting_pos {
                let world = self.entity.world.load();
                let (block, state) = world.get_block_and_state(&supporting);

                world
                    .block_registry
                    .on_entity_step(
                        block,
                        &world,
                        caller.as_ref() as &dyn EntityBase,
                        &supporting,
                        state,
                        false,
                    )
                    .await;

                // Check slightly below supporting_pos for additional supporting blocks (blocks under carpets and the like)
                if !block.is_solid() {
                    let below_supporting = supporting.down();
                    let (below_block, below_state) = world.get_block_and_state(&below_supporting);

                    // If block is not air, notify it as well
                    world
                        .block_registry
                        .on_entity_step(
                            below_block,
                            &world,
                            caller.as_ref() as &dyn EntityBase,
                            &below_supporting,
                            below_state,
                            true, // below supporting block
                        )
                        .await;
                }
            }

            self.tick_effects().await;

            // Current active item
            {
                let item_in_use = self.item_in_use.lock().await.clone();
                if let Some(item) = item_in_use.as_ref()
                    && self.item_use_time.fetch_sub(1, Ordering::Relaxed) <= 0
                {
                    // Consume item
                    let mut is_potion = false;
                    if let Some(food) = item.get_data_component::<FoodImpl>()
                        && let Some(player) = caller.get_player()
                    {
                        player
                            .hunger_manager
                            .eat(player, food.nutrition as u8, food.saturation)
                            .await;

                        // Special food effects
                        if item.item == &Item::GOLDEN_APPLE {
                            self.add_effect(pumpkin_data::potion::Effect {
                                effect_type: &pumpkin_data::effect::StatusEffect::REGENERATION,
                                amplifier: 1,
                                duration: 100,
                                ambient: false,
                                show_particles: true,
                                show_icon: true,
                                blend: false,
                            })
                            .await;
                            self.add_effect(pumpkin_data::potion::Effect {
                                effect_type: &pumpkin_data::effect::StatusEffect::ABSORPTION,
                                amplifier: 0,
                                duration: 2400,
                                ambient: false,
                                show_particles: true,
                                show_icon: true,
                                blend: false,
                            })
                            .await;
                        } else if item.item == &Item::ENCHANTED_GOLDEN_APPLE {
                            self.add_effect(pumpkin_data::potion::Effect {
                                effect_type: &pumpkin_data::effect::StatusEffect::REGENERATION,
                                amplifier: 1,
                                duration: 400,
                                ambient: false,
                                show_particles: true,
                                show_icon: true,
                                blend: false,
                            })
                            .await;
                            self.add_effect(pumpkin_data::potion::Effect {
                                effect_type: &pumpkin_data::effect::StatusEffect::ABSORPTION,
                                amplifier: 3,
                                duration: 2400,
                                ambient: false,
                                show_particles: true,
                                show_icon: true,
                                blend: false,
                            })
                            .await;
                            self.add_effect(pumpkin_data::potion::Effect {
                                effect_type: &pumpkin_data::effect::StatusEffect::RESISTANCE,
                                amplifier: 0,
                                duration: 6000,
                                ambient: false,
                                show_particles: true,
                                show_icon: true,
                                blend: false,
                            })
                            .await;
                            self.add_effect(pumpkin_data::potion::Effect {
                                effect_type: &pumpkin_data::effect::StatusEffect::FIRE_RESISTANCE,
                                amplifier: 0,
                                duration: 6000,
                                ambient: false,
                                show_particles: true,
                                show_icon: true,
                                blend: false,
                            })
                            .await;
                        }
                    }

                    // Handle potion consumption
                    if item.get_data_component::<pumpkin_data::data_component_impl::PotionContentsImpl>().is_some() {
                        let effects = crate::item::potion::PotionContents::read_potion_effects(item);
                        crate::item::potion::PotionContents::apply_effects_to(self, effects, 1.0, crate::item::potion::PotionApplicationSource::Normal).await;
                        is_potion = true;
                    }

                    if let Some(player) = caller.get_player() {
                        player
                            .trigger_advancement(crate::entity::player::advancement::trigger::AdvancementTrigger::ConsumeItem {
                                item_id: format!("minecraft:{}", item.item.registry_key),
                            })
                            .await;

                        // Prefer modifying the exact stack that matches the consumed item:
                        // 1) selected hotbar (held_item)
                        // 2) off-hand
                        // 3) fallback to active_hand if the above didn't match
                        let mut handled = false;

                        // Check main hand (hotbar selected)
                        let held_arc = player.inventory.held_item();
                        {
                            let mut held_lock = held_arc.lock().await;
                            if held_lock.are_items_and_components_equal(item) {
                                if is_potion {
                                    if player.gamemode.load() != GameMode::Creative {
                                        held_lock.decrement(1);
                                        if held_lock.is_empty() {
                                            *held_lock = ItemStack::new(1, &Item::GLASS_BOTTLE);
                                        }
                                    }
                                } else {
                                    held_lock.decrement_unless_creative(player.gamemode.load(), 1);
                                }
                                handled = true;
                            }
                        }

                        if !handled {
                            // Check off-hand
                            let off_arc = player.inventory.off_hand_item().await;
                            let mut off_lock = off_arc.lock().await;
                            if off_lock.are_items_and_components_equal(item) {
                                if is_potion {
                                    if player.gamemode.load() != GameMode::Creative {
                                        off_lock.decrement(1);
                                        if off_lock.is_empty() {
                                            *off_lock = ItemStack::new(1, &Item::GLASS_BOTTLE);
                                        }
                                    }
                                } else {
                                    off_lock.decrement_unless_creative(player.gamemode.load(), 1);
                                }

                                handled = true;
                            }
                        }

                        if !handled {
                            // Use stored active_hand (as a fallback)
                            let active_hand = *self.active_hand.lock().await;
                            let hand_to_modify = active_hand.unwrap_or(Hand::Right);
                            let item_stack = self
                                .get_stack_in_hand(caller.as_ref(), hand_to_modify)
                                .await;
                            let mut item_lock = item_stack.lock().await;

                            if is_potion {
                                if player.gamemode.load() != GameMode::Creative {
                                    item_lock.decrement(1);
                                    if item_lock.is_empty() {
                                        *item_lock = ItemStack::new(1, &Item::GLASS_BOTTLE);
                                    }
                                }
                            } else {
                                item_lock.decrement_unless_creative(player.gamemode.load(), 1);
                            }
                        }

                        if let Some(cooldown) = item.get_use_cooldown() {
                            let group = cooldown
                                .cooldown_group
                                .clone()
                                .unwrap_or_else(|| item.item.registry_key.to_string());
                            player
                                .start_cooldown(group, (cooldown.seconds * 20.0) as i32)
                                .await;
                        }
                    }

                    self.clear_active_hand().await;
                }
            }

            if self.hurt_cooldown.load(Relaxed) > 0 {
                self.hurt_cooldown.fetch_sub(1, Relaxed);
            }
            if self.health.load() <= 0.0 {
                let time = self.death_time.fetch_add(1, Relaxed);
                // Only send death particles once (on the exact tick death_time reaches 20)
                // and then remove the entity, preventing entity_event spam.
                if time == 20 && !self.entity.removed.swap(true, Ordering::Relaxed) {
                    self.entity
                        .removal_reason
                        .store(Some(crate::entity::RemovalReason::Killed));
                    self.entity
                        .world
                        .load()
                        .send_entity_status(&self.entity, EntityStatus::Death);
                    self.entity.remove().await;
                }
            }
        })
    }

    fn get_entity(&self) -> &Entity {
        &self.entity
    }

    fn get_living_entity(&self) -> Option<&LivingEntity> {
        Some(self)
    }

    fn is_pushable(&self) -> bool {
        self.health.load() > 0.0 && !self.dead.load(Relaxed)
    }

    fn cast_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_nbt_storage(&self) -> &dyn NBTStorage {
        self
    }
}
