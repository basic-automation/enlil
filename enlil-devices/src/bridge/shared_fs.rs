use std::fs;
use std::path::{Path, PathBuf};
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct SharedFsConfig {
    pub mount_point: PathBuf,
    pub backing_path: PathBuf,
    pub max_size: u64,
    pub auto_clean: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedFsLayout {
    Clipboard,
    Dragdrop,
    Transfer,
    User,
}

impl SharedFsLayout {
    const fn subdir(&self) -> &str {
        match self {
            Self::Clipboard => "clipboard",
            Self::Dragdrop => "dragdrop",
            Self::Transfer => "transfer",
            Self::User => "user",
        }
    }
}

pub struct FileTransfer {
    pub id: String,
    pub source_guest: String,
    pub dest_guest: String,
    pub files: Vec<PathBuf>,
    pub status: TransferStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferStatus {
    Pending,
    InProgress,
    Complete,
    Failed,
}

pub struct SharedFsManager {
    config: SharedFsConfig,
    transfers: HashMap<String, FileTransfer>,
}

impl SharedFsManager {
    /// Creates a new `SharedFsManager` with the given configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if the backing directory layout cannot be initialized.
    pub fn new(config: SharedFsConfig) -> std::io::Result<Self> {
        Self::init_layout(&config.backing_path)?;
        Ok(Self {
            config,
            transfers: HashMap::new(),
        })
    }

    fn init_layout(root: &Path) -> std::io::Result<()> {
        for layout in [
            SharedFsLayout::Clipboard,
            SharedFsLayout::Dragdrop,
            SharedFsLayout::Transfer,
            SharedFsLayout::User,
        ] {
            fs::create_dir_all(root.join(layout.subdir()))?;
        }
        Ok(())
    }

    /// Stages a file to the clipboard directory.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be copied to the clipboard directory.
    pub fn stage_clipboard(&self, src: &Path) -> std::io::Result<PathBuf> {
        let dest = self.config.backing_path
            .join(SharedFsLayout::Clipboard.subdir())
            .join(src.file_name().unwrap_or_default());
        fs::copy(src, &dest)?;
        Ok(dest)
    }

    /// Stages a file to the drag-drop directory.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be copied to the drag-drop directory.
    pub fn stage_dragdrop(&self, src: &Path) -> std::io::Result<PathBuf> {
        let dest = self.config.backing_path
            .join(SharedFsLayout::Dragdrop.subdir())
            .join(src.file_name().unwrap_or_default());
        fs::copy(src, &dest)?;
        Ok(dest)
    }

    pub fn add_transfer(&mut self, id: String, src_guest: String, dst_guest: String, files: Vec<PathBuf>) {
        let transfer_id = id.clone();
        self.transfers.insert(
            transfer_id,
            FileTransfer {
                id,
                source_guest: src_guest,
                dest_guest: dst_guest,
                files,
                status: TransferStatus::Pending,
            },
        );
    }

    #[must_use]
    pub fn get_transfer(&self, id: &str) -> Option<&FileTransfer> {
        self.transfers.get(id)
    }

    /// Cleans temporary directories by removing all files.
    ///
    /// # Errors
    ///
    /// Returns an error if directories cannot be read or files cannot be removed.
    pub fn clean_temp_dirs(&self) -> std::io::Result<()> {
        for layout in [
            SharedFsLayout::Clipboard,
            SharedFsLayout::Dragdrop,
            SharedFsLayout::Transfer,
        ] {
            let dir = self.config.backing_path.join(layout.subdir());
            if dir.exists() {
                for entry in fs::read_dir(&dir)? {
                    let entry = entry?;
                    let path = entry.path();
                    if path.is_file() {
                        fs::remove_file(path)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Checks the total space used by files in the backing directory.
    ///
    /// # Errors
    ///
    /// Returns an error if the backing directory cannot be read.
    pub fn check_space(&self) -> std::io::Result<u64> {
        let mut total = 0;
        for entry in fs::read_dir(&self.config.backing_path)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_file() {
                total += metadata.len();
            }
        }
        Ok(total)
    }

    /// Calculates the available space remaining before the max size is reached.
    ///
    /// # Errors
    ///
    /// Returns an error if space usage cannot be determined.
    pub fn available_space(&self) -> std::io::Result<u64> {
        let used = self.check_space()?;
        Ok(self.config.max_size.saturating_sub(used))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn setup_test(name: &str) -> (PathBuf, SharedFsConfig) {
        let root = std::env::temp_dir().join(format!("enlil_test_{name}"));
        let cfg = SharedFsConfig {
            mount_point: root.join("mnt"),
            backing_path: root.join("backing"),
            max_size: 1024 * 1024,
            auto_clean: true,
        };
        let _ = fs::remove_dir_all(&root);
        (root, cfg)
    }

    #[test]
    fn test_manager_creation() {
        let (root, cfg) = setup_test("manager_creation");
        let _mgr = SharedFsManager::new(cfg).unwrap();
        assert!(root.join("backing/user").exists());
        assert!(root.join("backing/clipboard").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_stage_clipboard() {
        let (root, cfg) = setup_test("stage_clipboard");
        let mgr = SharedFsManager::new(cfg).unwrap();
        let src = root.join("test.txt");
        fs::File::create(&src).unwrap().write_all(b"test").unwrap();
        let dest = mgr.stage_clipboard(&src).unwrap();
        assert!(dest.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_add_and_get_transfer() {
        let (root, cfg) = setup_test("add_and_get_transfer");
        let mut mgr = SharedFsManager::new(cfg).unwrap();
        mgr.add_transfer("t1".to_string(), "guest1".to_string(), "guest2".to_string(), vec![]);
        assert!(mgr.get_transfer("t1").is_some());
        assert_eq!(mgr.get_transfer("t1").unwrap().status, TransferStatus::Pending);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_clean_temp_dirs() {
        let (root, cfg) = setup_test("clean_temp_dirs");
        let mgr = SharedFsManager::new(cfg).unwrap();
        let temp = root.join("backing/clipboard/test.txt");
        fs::File::create(&temp).unwrap();
        mgr.clean_temp_dirs().unwrap();
        assert!(!temp.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_space_tracking() {
        let (root, cfg) = setup_test("space_tracking");
        let mgr = SharedFsManager::new(cfg).unwrap();
        let space = mgr.available_space().unwrap();
        assert_eq!(space, 1024 * 1024);
        let _ = fs::remove_dir_all(&root);
    }
}
