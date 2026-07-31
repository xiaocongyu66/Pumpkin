use crate::{
    entity::{NBTStorage, player::Player},
    server::Server,
};
use crossbeam::atomic::AtomicCell;
use pumpkin_inventory::screen_handler::ScreenHandler;
use pumpkin_nbt::compound::NbtCompound;
use pumpkin_world::data::player_data::{PlayerDataError, PlayerDataStorage};
use std::sync::Arc;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use tracing::{debug, error};

/// Helper for managing player data in the server context.
///
/// This struct provides server-wide access to the `PlayerDataStorage` and
/// convenience methods for player handling.
pub struct ServerPlayerData {
    storage: Arc<PlayerDataStorage>,
    save_interval: Duration,
    last_save: AtomicCell<Instant>,
}

impl ServerPlayerData {
    /// Creates a new `ServerPlayerData` with specified configuration.
    pub fn new(data_path: impl Into<PathBuf>, save_interval: Duration, enabled: bool) -> Self {
        Self {
            storage: Arc::new(PlayerDataStorage::new(data_path, enabled)),
            save_interval,
            last_save: AtomicCell::new(Instant::now()),
        }
    }

    /// Handles a player leaving the server.
    ///
    /// This function saves player data when they disconnect.
    ///
    /// # Arguments
    ///
    /// * `player` - The player who left.
    ///
    /// # Returns
    ///
    /// A Result indicating success or the error that occurred.
    pub async fn handle_player_leave(&self, player: &Arc<Player>) -> Result<(), PlayerDataError> {
        player
            .player_screen_handler
            .lock()
            .await
            .on_closed(player.as_ref())
            .await;
        player.on_handled_screen_closed().await;

        let mut nbt = NbtCompound::new();
        player.write_nbt(&mut nbt).await;

        let storage = self.storage.clone();
        let uuid = player.gameprofile.id;
        // Save to disk
        tokio::task::spawn_blocking(move || storage.save_player_data(&uuid, nbt))
            .await
            .expect("Player data save panicked")?;
        Ok(())
    }

    /// Performs periodic maintenance tasks.
    ///
    /// This function should be called regularly to save player data and clean
    /// expired cache entries.
    pub async fn tick(&self, server: &Server) -> Result<(), PlayerDataError> {
        let now = Instant::now();

        // Only save players periodically based on save_interval
        let last_save = self.last_save.load();
        let should_save = now.duration_since(last_save) >= self.save_interval;

        if should_save && self.storage.is_save_enabled() {
            self.last_save.store(now);
            // Save all online players periodically across all worlds
            for world in server.worlds.load().iter() {
                for player in world.players.load().iter() {
                    let mut nbt = NbtCompound::new();
                    player.write_nbt(&mut nbt).await;

                    let storage = self.storage.clone();
                    let uuid = player.gameprofile.id;
                    // Save to disk periodically to prevent data loss on server crash
                    if let Err(e) =
                        tokio::task::spawn_blocking(move || storage.save_player_data(&uuid, nbt))
                            .await
                            .expect("Player data periodic save panicked")
                    {
                        error!(
                            "Failed to save player data for {}: {e}",
                            player.gameprofile.id,
                        );
                    }
                }
            }

            debug!("Periodic player data save completed");
        }

        Ok(())
    }

    /// Saves all players' data immediately.
    ///
    /// This function immediately saves all online players' data to disk.
    /// Useful for server shutdown or backup operations.
    pub async fn save_all_players(&self, server: &Server) -> Result<(), PlayerDataError> {
        let mut total_players = 0;

        // Save players from all worlds.
        // 单个玩家保存失败只记日志继续，不能中断整轮保存 —— 否则第一个出错的
        // 玩家之后的所有人都不会被保存。对齐原版 PlayerList.saveAll 的裸循环 +
        // PlayerDataStorage.save 内部 catch 掉异常的行为。
        for world in server.worlds.load().iter() {
            for player in world.players.load().iter() {
                if let Err(e) = self.extract_data_and_save_player(player).await {
                    error!(
                        "Failed to save player data for {}: {e}",
                        player.gameprofile.id,
                    );
                } else {
                    total_players += 1;
                }
            }
        }

        debug!("Saved data for {total_players} online players");
        Ok(())
    }

    /// Loads player data and applies it to a player.
    ///
    /// This function loads a player's data and applies it to their Player instance.
    /// For new players, it creates default data without errors.
    ///
    /// # Arguments
    ///
    /// * `player` - The player to load data for and apply to.
    ///
    /// # Returns
    ///
    /// `Ok(None)` 表示这是新玩家（没有存档），`Err` 表示存档存在但读不出来。
    /// 调用方**必须**区分这两者：把读取失败当成新玩家会让空白数据覆盖掉原存档。
    pub async fn load_data(
        &self,
        uuid: &uuid::Uuid,
    ) -> Result<Option<NbtCompound>, PlayerDataError> {
        let storage = self.storage.clone();
        let uuid = *uuid;
        let result = tokio::task::spawn_blocking(move || storage.load_player_data(&uuid))
            .await
            .expect("Player data load panicked");

        // 存档存在却读不出来（且 .dat_old 也救不回来）时向上传播错误。
        // 这里绝不能返回 Ok(None)，否则玩家会带默认数据进服并覆盖原文件。
        result.inspect_err(|e| {
            error!("Error loading player data for {uuid}: {e}");
        })
    }

    /// Extracts and saves data from a player.
    ///
    /// This function extracts NBT data from a player and saves it to disk.
    ///
    /// # Arguments
    ///
    /// * `player` - The player to extract and save data for.
    ///
    /// # Returns
    ///
    /// A Result indicating success or the error that occurred.
    pub async fn extract_data_and_save_player(
        &self,
        player: &Player,
    ) -> Result<(), PlayerDataError> {
        if !self.storage.is_save_enabled() {
            return Ok(());
        }

        let uuid = player.gameprofile.id;
        let mut nbt = NbtCompound::new();
        player.write_nbt(&mut nbt).await;

        let storage = self.storage.clone();
        tokio::task::spawn_blocking(move || storage.save_player_data(&uuid, nbt))
            .await
            .expect("Player data extract and save panicked")?;

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use crate::data::player_server::ServerPlayerData;
    use pumpkin_nbt::compound::NbtCompound;
    use pumpkin_world::data::player_data::PlayerDataStorage;
    use std::time::Duration;
    use std::time::Instant;
    use tempfile::tempdir;
    use uuid::Uuid;

    #[tokio::test]
    async fn player_data_storage_new() {
        // Create a temporary directory for testing
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().to_path_buf();

        let storage = PlayerDataStorage::new(path.clone(), true);

        assert_eq!(storage.get_data_path().as_path(), path.as_path());
        // Note: save_enabled might be configured differently in your actual code
    }

    #[tokio::test]
    async fn player_data_storage_get_player_data_path() {
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().to_path_buf();

        let storage = PlayerDataStorage::new(path.clone(), true);

        let uuid = Uuid::new_v4();
        let expected_path = path.join(format!("{uuid}.dat"));

        assert_eq!(storage.get_player_data_path(&uuid), expected_path);
    }

    #[tokio::test]
    async fn player_data_storage_save_and_load() {
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().to_path_buf();

        let storage = PlayerDataStorage::new(path, true); // Ensure saving is enabled for this test

        let uuid = Uuid::new_v4();

        // Create test data
        let mut nbt = NbtCompound::new();
        nbt.put_string("TestKey", "TestValue".to_string());
        nbt.put_int("TestInt", 42);

        // Save the data
        storage.save_player_data(&uuid, nbt).unwrap();

        // Load the data
        let loaded_nbt = storage.load_player_data(&uuid).unwrap().unwrap();

        assert_eq!(loaded_nbt.get_string("TestKey").unwrap(), "TestValue");
        assert_eq!(loaded_nbt.get_int("TestInt").unwrap(), 42);
    }

    #[tokio::test]
    async fn player_data_storage_load_nonexistent() {
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().to_path_buf();

        let storage = PlayerDataStorage::new(path, true); // Ensure saving is enabled for this test

        let uuid = Uuid::new_v4();

        // Try to load non-existent data
        assert!(storage.load_player_data(&uuid).unwrap().is_none());
    }

    #[tokio::test]
    async fn player_data_storage_disabled() {
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().to_path_buf();

        let storage = PlayerDataStorage::new(path, false);

        let uuid = Uuid::new_v4();
        let mut nbt = NbtCompound::new();
        nbt.put_string("TestKey", "TestValue".to_string());

        // Save should succeed but do nothing
        let save_result = storage.save_player_data(&uuid, nbt);
        assert!(save_result.is_ok());

        // Load should report no data
        assert!(storage.load_player_data(&uuid).unwrap().is_none());
    }

    #[tokio::test]
    async fn server_player_data_new() {
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().to_path_buf();
        let save_interval = Duration::from_mins(5);

        let player_data = ServerPlayerData::new(path, save_interval, true);

        assert_eq!(player_data.save_interval, save_interval);
        assert!(
            Instant::now().duration_since(player_data.last_save.load()) < Duration::from_secs(1)
        );
    }

    #[tokio::test]
    async fn player_data_file_structure() {
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().to_path_buf();

        let uuid = Uuid::new_v4();
        let storage = PlayerDataStorage::new(path, true);

        // Create and save player data
        let mut nbt = NbtCompound::new();
        nbt.put_string("name", "TestPlayer".to_string());
        nbt.put_int("level", 42);
        storage.save_player_data(&uuid, nbt).unwrap();

        // Verify the file exists
        let player_data_path = storage.get_player_data_path(&uuid);
        assert!(player_data_path.exists());

        // Load it again and verify content
        let loaded_data = storage.load_player_data(&uuid).unwrap().unwrap();
        assert_eq!(loaded_data.get_string("name").unwrap(), "TestPlayer");
        assert_eq!(loaded_data.get_int("level").unwrap(), 42);
    }

    #[tokio::test]
    async fn second_save_keeps_previous_content_as_dat_old() {
        let temp_dir = tempdir().unwrap();
        let storage = PlayerDataStorage::new(temp_dir.path().to_path_buf(), true);
        let uuid = Uuid::new_v4();

        let mut first = NbtCompound::new();
        first.put_int("generation", 1);
        storage.save_player_data(&uuid, first).unwrap();

        let mut second = NbtCompound::new();
        second.put_int("generation", 2);
        storage.save_player_data(&uuid, second).unwrap();

        // 新内容进 .dat，上一份留在 .dat_old，并且不留临时文件。
        assert!(storage.get_player_backup_path(&uuid).is_file());
        assert_eq!(
            storage
                .load_player_data(&uuid)
                .unwrap()
                .unwrap()
                .get_int("generation")
                .unwrap(),
            2
        );
        let leftover_tmp = std::fs::read_dir(temp_dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!leftover_tmp);
    }

    #[tokio::test]
    async fn corrupted_dat_falls_back_to_dat_old_and_keeps_a_corrupted_copy() {
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().to_path_buf();
        let storage = PlayerDataStorage::new(path.clone(), true);
        let uuid = Uuid::new_v4();

        let mut good = NbtCompound::new();
        good.put_int("generation", 1);
        storage.save_player_data(&uuid, good).unwrap();
        // 再存一次，让第一份内容进入 .dat_old。
        let mut newer = NbtCompound::new();
        newer.put_int("generation", 2);
        storage.save_player_data(&uuid, newer).unwrap();

        // 模拟崩溃留下的半截文件。
        std::fs::write(storage.get_player_data_path(&uuid), b"truncated garbage").unwrap();

        // 应该回退到 .dat_old 里的第一代数据，而不是报告「没有存档」。
        let recovered = storage.load_player_data(&uuid).unwrap().unwrap();
        assert_eq!(recovered.get_int("generation").unwrap(), 1);

        // 损坏的文件被另存为 <uuid>_corrupted_<时间戳>.dat，没有被直接删掉。
        let corrupted_marker = format!("{uuid}_corrupted_");
        let has_corrupted_copy = std::fs::read_dir(&path)
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&corrupted_marker)
            });
        assert!(has_corrupted_copy);
    }

    #[tokio::test]
    async fn unreadable_dat_without_backup_is_an_error_not_a_new_player() {
        let temp_dir = tempdir().unwrap();
        let storage = PlayerDataStorage::new(temp_dir.path().to_path_buf(), true);
        let uuid = Uuid::new_v4();

        // 只有一个损坏的 .dat，没有 .dat_old 可以回退。
        std::fs::write(storage.get_player_data_path(&uuid), b"truncated garbage").unwrap();

        // 必须是 Err —— 若变成 Ok(None) 就会被当成新玩家并覆盖存档。
        assert!(storage.load_player_data(&uuid).is_err());
    }
}
