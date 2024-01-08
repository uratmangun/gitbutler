use anyhow::{Context, Result};

pub struct DirWriter {
    root: std::path::PathBuf,
}

impl DirWriter {
    pub fn open(root: std::path::PathBuf) -> Self {
        Self { root }
    }
}

impl DirWriter {
    fn write(&self, path: &str, contents: &[u8]) -> Result<()> {
        let file_path = self.root.join(path);
        let dir_path = file_path.parent().context("failed to get parent")?;
        std::fs::create_dir_all(dir_path).context("failed to create directory")?;
        std::fs::write(file_path, contents)?;
        Ok(())
    }

    pub fn remove(&self, path: &str) -> Result<()> {
        let file_path = self.root.join(path);
        if file_path.is_dir() {
            match std::fs::remove_dir_all(file_path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.into()),
            }
        } else {
            match std::fs::remove_file(file_path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.into()),
            }
        }
    }

    pub fn write_usize(&self, path: &str, contents: &usize) -> Result<()> {
        self.write_string(path, &contents.to_string())
    }

    pub fn write_u128(&self, path: &str, contents: &u128) -> Result<()> {
        self.write_string(path, &contents.to_string())
    }

    pub fn write_bool(&self, path: &str, contents: &bool) -> Result<()> {
        self.write_string(path, &contents.to_string())
    }

    pub fn write_string(&self, path: &str, contents: &str) -> Result<()> {
        self.write(path, contents.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_write() {
        let root = tempfile::tempdir().unwrap();
        let writer = DirWriter::open(root.path().to_path_buf());
        writer.write("foo/bar", b"baz").unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("foo/bar")).unwrap(),
            "baz"
        );
    }

    #[test]
    fn test_remove() {
        let root = tempfile::tempdir().unwrap();
        let writer = DirWriter::open(root.path().to_path_buf());
        writer.remove("foo/bar").unwrap();
        assert!(!root.path().join("foo/bar").exists());
        writer.write("foo/bar", b"baz").unwrap();
        writer.remove("foo/bar").unwrap();
        assert!(!root.path().join("foo/bar").exists());
        writer.write("parent/child", b"baz").unwrap();
        writer.remove("parent").unwrap();
        assert!(!root.path().join("parent").exists());
    }
}
