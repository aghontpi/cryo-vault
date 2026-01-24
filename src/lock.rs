use anyhow::{Context, Result};
use fs2::FileExt;
use std::fs::{self, File};
use std::path::Path;
use std::thread;
use std::time::Duration;

pub struct CryoLock {
    file: File,
}

impl CryoLock {
    /// Attempts to acquire an exclusive lock on the database.
    ///
    /// This will block for up to `timeout_ms` milliseconds.
    ///
    /// The function performs the following steps:
    /// 1. Ensures the database directory exists.
    /// 2. Opens the lock file.
    /// 3. Retries acquiring an exclusive lock in a loop until successful or timed out.
    pub fn acquire(db_path: &Path, timeout_ms: u64) -> Result<Self> {
        let lock_path = db_path.join("cryo.lock");

        if !db_path.exists() {
            fs::create_dir_all(db_path)?;
        }

        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .context("Failed to open lock file")?;

        let start = std::time::Instant::now();
        loop {
            match file.try_lock_exclusive() {
                Ok(_) => {
                    return Ok(CryoLock { file });
                }
                Err(_) => {
                    if start.elapsed().as_millis() as u64 > timeout_ms {
                        return Err(anyhow::anyhow!("Timed out waiting for lock"));
                    }
                    thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }
}

impl Drop for CryoLock {
    /// Automatically unlocks the file when the `CryoLock` instance is dropped.
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_lock_acquire_and_release() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path();

        // Acquire lock
        let lock = CryoLock::acquire(path, 100).expect("Failed to acquire lock");

        // Try to acquire again (should fail/timeout)
        let result = CryoLock::acquire(path, 100);
        assert!(result.is_err()); // Should timeout because 'lock' holds it

        // Drop lock
        drop(lock);

        // Acquire again (should succeed)
        let lock2 = CryoLock::acquire(path, 100);
        assert!(lock2.is_ok());
    }

    #[test]
    fn test_lock_creates_dir() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("subdir");

        assert!(!db_path.exists());
        let _lock = CryoLock::acquire(&db_path, 100).expect("Should create dir");
        assert!(db_path.exists());
    }
}
