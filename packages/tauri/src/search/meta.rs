use std::path;

use anyhow::Result;

use crate::{paths::DataDir, sessions::SessionId, storage};

use super::index;

#[derive(Clone)]
pub struct Storage {
    storage: storage::Storage,
}

impl From<&DataDir> for Storage {
    fn from(value: &DataDir) -> Self {
        Self {
            storage: storage::Storage::from(value),
        }
    }
}

impl Storage {
    pub fn delete_all(&self) -> Result<()> {
        self.storage.delete(
            path::Path::new("indexes")
                .join(format!("v{}", index::VERSION))
                .join("meta"),
        )?;
        Ok(())
    }

    pub fn get(&self, project_id: &str, session_id: &SessionId) -> Result<Option<u64>> {
        let filepath = path::Path::new("indexes")
            .join(format!("v{}", index::VERSION))
            .join("meta")
            .join(project_id)
            .join(session_id.to_string());
        let meta = match self.storage.read(filepath.to_str().unwrap())? {
            None => None,
            Some(meta) => meta.parse::<u64>().ok(),
        };
        Ok(meta)
    }

    pub fn set(&self, project_id: &str, session_id: &SessionId, version: u64) -> Result<()> {
        let filepath = path::Path::new("indexes")
            .join(format!("v{}", index::VERSION))
            .join("meta")
            .join(project_id)
            .join(session_id.to_string());
        self.storage.write(filepath, &version.to_string())?;
        Ok(())
    }
}
