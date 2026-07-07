use fs2::FileExt;
use serde_json::Value;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub struct ProcessLock {
    metadata_path: PathBuf,
    guard_path: PathBuf,
    guard_file: Option<File>,
}

impl ProcessLock {
    pub fn acquire(path: impl AsRef<Path>, payload: Value) -> Result<Self, String> {
        let metadata_path = path.as_ref().to_path_buf();
        let guard_path = guard_path_for(&metadata_path)?;
        let mut guard_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&guard_path)
            .map_err(|e| format!("open process lock failed: {}: {e}", guard_path.display()))?;

        if let Err(err) = guard_file.try_lock_exclusive() {
            let existing = read_lock_payload(&metadata_path);
            return Err(format!(
                "rsduck process lock is held: {}; existing={}; error={err}",
                metadata_path.display(),
                existing.unwrap_or_else(|| "<unreadable>".to_string())
            ));
        }

        let payload = serde_json::to_vec_pretty(&payload)
            .map_err(|e| format!("serialize process lock payload failed: {e}"))?;
        let mut metadata_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&metadata_path)
            .map_err(|e| {
                format!(
                    "open process lock metadata failed: {}: {e}",
                    metadata_path.display()
                )
            })?;
        metadata_file.write_all(&payload).map_err(|e| {
            format!(
                "write process lock failed: {}: {e}",
                metadata_path.display()
            )
        })?;
        metadata_file.write_all(b"\n").map_err(|e| {
            format!(
                "write process lock newline failed: {}: {e}",
                metadata_path.display()
            )
        })?;
        metadata_file
            .sync_all()
            .map_err(|e| format!("sync process lock failed: {}: {e}", metadata_path.display()))?;
        guard_file.set_len(0).map_err(|e| {
            format!(
                "truncate process lock guard failed: {}: {e}",
                guard_path.display()
            )
        })?;
        guard_file.seek(SeekFrom::Start(0)).map_err(|e| {
            format!(
                "seek process lock guard failed: {}: {e}",
                guard_path.display()
            )
        })?;
        guard_file.write_all(b"locked\n").map_err(|e| {
            format!(
                "write process lock guard failed: {}: {e}",
                guard_path.display()
            )
        })?;
        guard_file.sync_all().map_err(|e| {
            format!(
                "sync process lock guard failed: {}: {e}",
                guard_path.display()
            )
        })?;

        Ok(Self {
            metadata_path,
            guard_path,
            guard_file: Some(guard_file),
        })
    }
}

impl Drop for ProcessLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.metadata_path);
        if let Some(file) = self.guard_file.take() {
            let _ = file.unlock();
            drop(file);
        }
        let _ = std::fs::remove_file(&self.guard_path);
    }
}

fn read_lock_payload(path: &Path) -> Option<String> {
    let mut payload = String::new();
    File::open(path).ok()?.read_to_string(&mut payload).ok()?;
    let payload = payload.trim();
    if payload.is_empty() {
        None
    } else {
        Some(payload.to_string())
    }
}

fn guard_path_for(metadata_path: &Path) -> Result<PathBuf, String> {
    let file_name = metadata_path
        .file_name()
        .map(|value| value.to_string_lossy())
        .ok_or_else(|| {
            format!(
                "process lock path has no file name: {}",
                metadata_path.display()
            )
        })?;
    Ok(metadata_path.with_file_name(format!("{file_name}.guard")))
}

#[cfg(test)]
mod tests {
    use super::ProcessLock;
    use serde_json::json;

    #[test]
    fn process_lock_creates_and_removes_lock_file() {
        let path = std::env::temp_dir().join(format!(
            "rsduck_lock_test_{}_{}",
            std::process::id(),
            chrono::Local::now()
                .timestamp_nanos_opt()
                .unwrap_or_default()
        ));

        {
            let _lock = ProcessLock::acquire(&path, json!({"pid": 1, "mode": "test"})).unwrap();
            let payload = std::fs::read_to_string(&path).unwrap();
            assert!(payload.contains("\"pid\": 1"));
            assert!(payload.contains("\"mode\": \"test\""));
            assert!(path.exists());
        }

        assert!(!path.exists());
    }
}
