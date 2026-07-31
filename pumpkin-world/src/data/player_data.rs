use pumpkin_nbt::compound::NbtCompound;
use std::fs::{self, File, create_dir_all};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, error, warn};
use uuid::Uuid;

/// Manages the storage and retrieval of player data from disk and memory cache.
///
/// This struct provides functions to load and save player data to/from NBT files,
/// with a memory cache to handle player disconnections temporarily.
pub struct PlayerDataStorage {
    /// Path to the directory where player data is stored
    data_path: PathBuf,
    /// Whether player data saving is enabled
    save_enabled: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum PlayerDataError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("NBT error: {0}")]
    Nbt(String),
}

impl PlayerDataStorage {
    /// Creates a new `PlayerDataStorage` with the specified data path and cache expiration time.
    pub fn new(data_path: impl Into<PathBuf>, enabled: bool) -> Self {
        let path = data_path.into();
        if !path.exists()
            && let Err(e) = create_dir_all(&path)
        {
            error!(
                "Failed to create player data directory at {}: {e}",
                path.display()
            );
        }

        Self {
            data_path: path,
            save_enabled: enabled,
        }
    }

    #[must_use]
    pub const fn get_data_path(&self) -> &PathBuf {
        &self.data_path
    }

    #[must_use]
    pub const fn is_save_enabled(&self) -> bool {
        self.save_enabled
    }

    pub const fn set_save_enabled(&mut self, enabled: bool) {
        self.save_enabled = enabled;
    }

    /// Returns the path for a player's data file based on their UUID.
    #[must_use]
    pub fn get_player_data_path(&self, uuid: &Uuid) -> PathBuf {
        self.get_data_path().join(format!("{uuid}.dat"))
    }

    /// 返回玩家存档的备份文件路径（对齐原版的 `.dat_old`）。
    #[must_use]
    pub fn get_player_backup_path(&self, uuid: &Uuid) -> PathBuf {
        self.get_data_path().join(format!("{uuid}.dat_old"))
    }

    /// 读取单个存档文件，文件不存在时返回 `Ok(None)`。
    ///
    /// 「文件不存在」与「读取失败」在这里就被区分开：前者是 `Ok(None)`，
    /// 后者是 `Err`，绝不能把后者降级成前者，否则会让玩家变成新号。
    fn read_data_file(path: &Path) -> Result<Option<NbtCompound>, PlayerDataError> {
        if !path.is_file() {
            return Ok(None);
        }

        // 用 fs::read 一次读入内存，再交给 NBT 解析（需要 Seek）。
        let bytes = fs::read(path)?;
        pumpkin_nbt::nbt_compress::read_gzip_compound_tag(io::Cursor::new(bytes))
            .map(Some)
            .map_err(|e| PlayerDataError::Nbt(e.to_string()))
    }

    /// 把损坏的存档另存一份，命名为 `<uuid>_corrupted_<时间戳>.dat`。
    ///
    /// 对齐原版 `PlayerDataStorage.backup`：用**复制**而不是移动，
    /// 这样原文件仍留在原地，管理员可以两边都拿到。
    fn backup_corrupted(&self, uuid: &Uuid, suffix: &str) {
        let source = self.get_data_path().join(format!("{uuid}{suffix}"));
        if !source.is_file() {
            return;
        }

        // 复用项目里已有的时间戳取法（见 poi/mod.rs），不引入新依赖。
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let target = self
            .get_data_path()
            .join(format!("{uuid}_corrupted_{timestamp}{suffix}"));

        if let Err(e) = fs::copy(&source, &target) {
            warn!(
                "Failed to back up corrupted player data for {uuid}: {e} ({} -> {})",
                source.display(),
                target.display()
            );
        } else {
            warn!(
                "Backed up corrupted player data for {uuid} to {}",
                target.display()
            );
        }
    }

    /// Loads player data from the `.dat` file, falling back to `.dat_old`.
    ///
    /// 回退链对齐原版 `PlayerDataStorage.load`：
    /// 1. 读 `.dat`，成功即返回；
    /// 2. 失败则把损坏文件备份为 `<uuid>_corrupted_<时间戳>.dat`；
    /// 3. 再尝试读 `.dat_old`，成功则返回（同时告警）；
    /// 4. 两者都读失败才向上报错 —— **不是**「没有存档」。
    ///
    /// # Arguments
    ///
    /// * `uuid` - The UUID of the player to load data for.
    ///
    /// # Returns
    ///
    /// `Ok(None)` 表示确实没有存档（新玩家），`Ok(Some(..))` 表示读到了数据，
    /// `Err` 表示存档存在但读不出来 —— 调用方必须区别对待后两者。
    pub fn load_player_data(&self, uuid: &Uuid) -> Result<Option<NbtCompound>, PlayerDataError> {
        // If player data saving is disabled, return empty data
        if !self.is_save_enabled() {
            return Ok(None);
        }

        let path = self.get_player_data_path(uuid);
        let backup_path = self.get_player_backup_path(uuid);

        let primary_error = match Self::read_data_file(&path) {
            Ok(Some(nbt)) => {
                debug!("Loaded player data for {uuid} from disk");
                return Ok(Some(nbt));
            }
            // 主文件不存在：不算损坏，直接看 .dat_old 是否留有内容。
            Ok(None) => None,
            Err(e) => {
                error!("Failed to read player data for {uuid}: {e}");
                // 主文件读失败 —— 先留一份损坏样本再试备份。
                self.backup_corrupted(uuid, ".dat");
                Some(e)
            }
        };

        match Self::read_data_file(&backup_path) {
            Ok(Some(nbt)) => {
                warn!("Recovered player data for {uuid} from the .dat_old backup");
                Ok(Some(nbt))
            }
            // 主文件损坏但备份也没有 → 报错，绝不能当成新玩家。
            Ok(None) => {
                if let Some(e) = primary_error {
                    return Err(e);
                }
                debug!("No player data file found for {uuid}");
                Ok(None)
            }
            Err(backup_error) => {
                error!("Failed to read backup player data for {uuid}: {backup_error}");
                // 两个都坏了，优先上报主文件的错误。
                Err(primary_error.unwrap_or(backup_error))
            }
        }
    }

    /// Saves player data to an NBT file atomically.
    ///
    /// 写入步骤对齐原版 `Util.safeReplaceFile`：
    /// 1. 写同目录的临时文件（同一文件系统 rename 才是原子的）；
    /// 2. `sync_all` 落盘，保证 rename 之后内容一定完整；
    /// 3. 已有的 `.dat` 先挪成 `.dat_old` 作为备份；
    /// 4. 临时文件原子 rename 成 `.dat`；失败则把 `.dat_old` 回滚。
    ///
    /// # Arguments
    ///
    /// * `uuid` - The UUID of the player to save data for.
    /// * `data` - The NBT compound data to save.
    ///
    /// # Returns
    ///
    /// A Result indicating success or the error that occurred.
    pub fn save_player_data(&self, uuid: &Uuid, data: NbtCompound) -> Result<(), PlayerDataError> {
        // Skip saving if disabled in config
        if !self.is_save_enabled() {
            return Ok(());
        }

        let path = self.get_player_data_path(uuid);
        let backup_path = self.get_player_backup_path(uuid);

        // Ensure parent directory exists
        if let Some(parent) = path.parent()
            && let Err(e) = create_dir_all(parent)
        {
            error!("Failed to create player data directory for {uuid}: {e}");
            return Err(PlayerDataError::Io(e));
        }

        // 临时文件必须和目标同目录，否则 rename 可能跨文件系统而失去原子性。
        let temporary_path = self
            .get_data_path()
            .join(format!("{uuid}.dat.{}.tmp", std::process::id()));

        if let Err(e) = Self::write_temporary(&temporary_path, data) {
            // 临时文件写坏了不影响既有存档，清掉残留即可。
            let _ = fs::remove_file(&temporary_path);
            return Err(e);
        }

        // 保留上一份存档为 .dat_old（原版 safeReplaceFile 的第一步）。
        if path.exists() {
            if backup_path.exists() {
                fs::remove_file(&backup_path)?;
            }
            fs::rename(&path, &backup_path)?;
        }

        if let Err(e) = fs::rename(&temporary_path, &path) {
            // rename 失败时回滚备份，别让玩家连旧存档都没了。
            if !path.exists() && backup_path.exists() {
                let _ = fs::rename(&backup_path, &path);
            }
            let _ = fs::remove_file(&temporary_path);
            error!("Failed to replace player data file for {uuid}: {e}");
            return Err(PlayerDataError::Io(e));
        }

        debug!("Saved player data for {uuid} to disk");
        Ok(())
    }

    /// 写临时文件并 fsync，确保 rename 之后磁盘上的内容是完整的。
    fn write_temporary(temporary_path: &Path, data: NbtCompound) -> Result<(), PlayerDataError> {
        let file = File::create(temporary_path)?;
        pumpkin_nbt::nbt_compress::write_gzip_compound_tag(data, &file)
            .map_err(|e| PlayerDataError::Nbt(e.to_string()))?;
        file.sync_all()?;
        Ok(())
    }
}
