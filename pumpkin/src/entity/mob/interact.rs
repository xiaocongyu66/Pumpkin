use super::MobEntity;
use crate::entity::player::Player;
use crate::entity::{Entity, EntityBase};
use pumpkin_data::item_stack::ItemStack;
use pumpkin_data::sound::Sound;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

impl MobEntity {
    pub async fn mob_interact(&self, player: &Arc<Player>, item_stack: &mut ItemStack) -> bool {
        let entity = &self.living_entity.entity;

        // 原版 `Entity.interact`（Entity.java:2203-2216）：只有当 holder 就是这名玩家时，
        // 右键才解绳；创造模式走 `removeLeash()`（不掉 lead），否则 `dropLeash()`。
        let leashed_to_this_player = {
            let guard = entity.leashed_to.lock().await;
            guard
                .as_ref()
                .is_some_and(|holder| holder.get_entity().entity_id == player.entity_id())
        };

        if leashed_to_this_player {
            let drop_lead = player.gamemode.load() != pumpkin_util::GameMode::Creative;
            entity.drop_leash_inherent(drop_lead).await;
            // 原版 Mob.onLeashRemoved（Mob.java:1295-1300）：解绳后若不再被拴则 clearHome()。
            self.position_target_range.store(-1, Relaxed);
            entity.play_sound(Sound::ItemLeadUntied);
            return true;
        }

        // If holding a lead, leash the mob to the player
        if item_stack.item.registry_key == "lead"
            || item_stack.item.registry_key == "minecraft:lead"
        {
            let diff = entity.pos.load() - player.get_entity().pos.load();
            let dist_sq = diff.length_squared();
            if dist_sq <= Entity::LEASH_SNAP_DISTANCE * Entity::LEASH_SNAP_DISTANCE {
                entity.leash_to(player.clone() as Arc<dyn EntityBase>).await;
                if player.gamemode.load() != pumpkin_util::GameMode::Creative {
                    item_stack.decrement(1);
                }
                return true;
            }
        }

        false
    }
}
