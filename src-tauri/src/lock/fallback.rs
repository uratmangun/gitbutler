use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use std::time::Duration;

use backoff::{retry, ExponentialBackoff};
use tracing::instrument;

pub struct FileLock {
    path: PathBuf,
    _file: File,
}

impl FileLock {
    pub fn lock(path: PathBuf) -> FileLock {
        let mut options = OpenOptions::new();
        options.create_new(true);
        options.write(true);
        let try_write_lock_file = || match options.open(&path) {
            Ok(file) => Ok(FileLock {
                path: path.clone(),
                _file: file,
            }),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(backoff::Error::Transient {
                    err,
                    retry_after: None,
                })
            }
            Err(err) if cfg!(windows) && err.kind() == std::io::ErrorKind::PermissionDenied => {
                Err(backoff::Error::Transient {
                    err,
                    retry_after: None,
                })
            }
            Err(err) => Err(backoff::Error::Permanent(err)),
        };
        let backoff = ExponentialBackoff {
            initial_interval: Duration::from_millis(1),
            max_elapsed_time: Some(Duration::from_secs(10)),
            ..Default::default()
        };
        match retry(backoff, try_write_lock_file) {
            Err(err) => panic!(
                "failed to create lock file {}: {}",
                path.to_string_lossy(),
                err
            ),
            Ok(file_lock) => file_lock,
        }
    }
}

impl Drop for FileLock {
    #[instrument(skip_all)]
    fn drop(&mut self) {
        std::fs::remove_file(&self.path).expect("failed to delete lock file");
    }
}
