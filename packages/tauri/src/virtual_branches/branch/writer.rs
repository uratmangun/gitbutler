use anyhow::{Context, Result};

use crate::{
    gb_repository,
    writer::{self, Writer},
};

use super::Branch;

pub struct BranchWriter<'writer> {
    repository: &'writer gb_repository::Repository,
    writer: writer::DirWriter,
}

impl<'writer> BranchWriter<'writer> {
    pub fn new(repository: &'writer gb_repository::Repository) -> Self {
        Self {
            repository,
            writer: writer::DirWriter::open(repository.root()),
        }
    }

    pub fn delete(&self, branch: &Branch) -> Result<()> {
        self.repository.mark_active_session()?;

        let _lock = self.repository.lock();
        self.writer.remove(&format!("branches/{}", branch.id))?;
        Ok(())
    }

    pub fn write(&self, branch: &Branch) -> Result<()> {
        self.repository.mark_active_session()?;

        let _lock = self.repository.lock();

        self.writer
            .write_string(
                &format!("branches/{}/id", branch.id),
                &branch.id.to_string(),
            )
            .context("Failed to write branch id")?;

        self.writer
            .write_string(&format!("branches/{}/meta/name", branch.id), &branch.name)
            .context("Failed to write branch name")?;

        self.writer
            .write_string(&format!("branches/{}/meta/notes", branch.id), &branch.notes)
            .context("Failed to write notes")?;

        self.writer
            .write_usize(&format!("branches/{}/meta/order", branch.id), &branch.order)
            .context("Failed to write branch order")?;

        self.writer
            .write_bool(
                &format!("branches/{}/meta/applied", branch.id),
                &branch.applied,
            )
            .context("Failed to write branch applied")?;
        if let Some(upstream) = &branch.upstream {
            self.writer
                .write_string(
                    &format!("branches/{}/meta/upstream", branch.id),
                    &upstream.to_string(),
                )
                .context("Failed to write branch upstream")?;
        };
        if let Some(upstream_head) = &branch.upstream_head {
            self.writer
                .write_string(
                    &format!("branches/{}/meta/upstream_head", branch.id),
                    &upstream_head.to_string(),
                )
                .context("Failed to write branch upstream head")?;
        }
        self.writer
            .write_string(
                &format!("branches/{}/meta/tree", branch.id),
                &branch.tree.to_string(),
            )
            .context("Failed to write branch tree")?;
        self.writer
            .write_string(
                &format!("branches/{}/meta/head", branch.id),
                &branch.head.to_string(),
            )
            .context("Failed to write branch head")?;
        self.writer
            .write_u128(
                &format!("branches/{}/meta/created_timestamp_ms", branch.id),
                &branch.created_timestamp_ms,
            )
            .context("Failed to write branch created timestamp")?;
        self.writer
            .write_u128(
                &format!("branches/{}/meta/updated_timestamp_ms", branch.id),
                &branch.updated_timestamp_ms,
            )
            .context("Failed to write branch updated timestamp")?;

        self.writer
            .write_string(
                &format!("branches/{}/meta/ownership", branch.id),
                &branch.ownership.to_string(),
            )
            .context("Failed to write branch ownership")?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use once_cell::sync::Lazy;

    use crate::{
        test_utils::{Case, Suite},
        virtual_branches::branch,
    };

    use self::branch::BranchId;

    use super::*;

    static TEST_INDEX: Lazy<AtomicUsize> = Lazy::new(|| AtomicUsize::new(0));

    fn test_branch() -> Branch {
        TEST_INDEX.fetch_add(1, Ordering::Relaxed);

        Branch {
            id: BranchId::generate(),
            name: format!("branch_name_{}", TEST_INDEX.load(Ordering::Relaxed)),
            notes: String::new(),
            applied: true,
            upstream: Some(
                format!(
                    "refs/remotes/origin/upstream_{}",
                    TEST_INDEX.load(Ordering::Relaxed)
                )
                .parse()
                .unwrap(),
            ),
            upstream_head: None,
            created_timestamp_ms: TEST_INDEX.load(Ordering::Relaxed) as u128,
            updated_timestamp_ms: (TEST_INDEX.load(Ordering::Relaxed) + 100) as u128,
            head: format!(
                "0123456789abcdef0123456789abcdef0123456{}",
                TEST_INDEX.load(Ordering::Relaxed)
            )
            .parse()
            .unwrap(),
            tree: format!(
                "0123456789abcdef0123456789abcdef012345{}",
                TEST_INDEX.load(Ordering::Relaxed) + 10
            )
            .parse()
            .unwrap(),
            ownership: branch::Ownership {
                files: vec![branch::FileOwnership {
                    file_path: format!("file/{}", TEST_INDEX.load(Ordering::Relaxed)).into(),
                    hunks: vec![],
                }],
            },
            order: TEST_INDEX.load(Ordering::Relaxed),
        }
    }

    #[test]
    fn test_write_branch() -> Result<()> {
        let Case { gb_repository, .. } = Suite::default().new_case();

        let branch = test_branch();

        let writer = BranchWriter::new(&gb_repository);
        writer.write(&branch)?;

        let root = gb_repository
            .root()
            .join("branches")
            .join(branch.id.to_string());

        assert_eq!(
            fs::read_to_string(root.join("meta").join("name").to_str().unwrap())
                .context("Failed to read branch name")?,
            branch.name
        );
        assert_eq!(
            fs::read_to_string(root.join("meta").join("applied").to_str().unwrap())?
                .parse::<bool>()
                .context("Failed to read branch applied")?,
            branch.applied
        );
        assert_eq!(
            fs::read_to_string(root.join("meta").join("upstream").to_str().unwrap())
                .context("Failed to read branch upstream")?,
            branch.upstream.clone().unwrap().to_string()
        );
        assert_eq!(
            fs::read_to_string(
                root.join("meta")
                    .join("created_timestamp_ms")
                    .to_str()
                    .unwrap()
            )
            .context("Failed to read branch created timestamp")?
            .parse::<u128>()
            .context("Failed to parse branch created timestamp")?,
            branch.created_timestamp_ms
        );
        assert_eq!(
            fs::read_to_string(
                root.join("meta")
                    .join("updated_timestamp_ms")
                    .to_str()
                    .unwrap()
            )
            .context("Failed to read branch updated timestamp")?
            .parse::<u128>()
            .context("Failed to parse branch updated timestamp")?,
            branch.updated_timestamp_ms
        );

        writer.delete(&branch)?;
        fs::read_dir(root).unwrap();

        Ok(())
    }

    #[test]
    fn test_should_create_session() -> Result<()> {
        let Case { gb_repository, .. } = Suite::default().new_case();

        let branch = test_branch();

        let writer = BranchWriter::new(&gb_repository);
        writer.write(&branch)?;

        assert!(gb_repository.get_current_session()?.is_some());

        Ok(())
    }

    #[test]
    fn test_should_update() -> Result<()> {
        let Case { gb_repository, .. } = Suite::default().new_case();

        let branch = test_branch();

        let writer = BranchWriter::new(&gb_repository);
        writer.write(&branch)?;

        let updated_branch = Branch {
            name: "updated_name".to_string(),
            applied: false,
            upstream: Some("refs/remotes/origin/upstream_updated".parse().unwrap()),
            created_timestamp_ms: 2,
            updated_timestamp_ms: 3,
            ownership: branch::Ownership { files: vec![] },
            ..branch.clone()
        };

        writer.write(&updated_branch)?;

        let root = gb_repository
            .root()
            .join("branches")
            .join(branch.id.to_string());

        assert_eq!(
            fs::read_to_string(root.join("meta").join("name").to_str().unwrap())
                .context("Failed to read branch name")?,
            updated_branch.name
        );
        assert_eq!(
            fs::read_to_string(root.join("meta").join("applied").to_str().unwrap())?
                .parse::<bool>()
                .context("Failed to read branch applied")?,
            updated_branch.applied
        );
        assert_eq!(
            fs::read_to_string(root.join("meta").join("upstream").to_str().unwrap())
                .context("Failed to read branch upstream")?,
            updated_branch.upstream.unwrap().to_string()
        );
        assert_eq!(
            fs::read_to_string(
                root.join("meta")
                    .join("created_timestamp_ms")
                    .to_str()
                    .unwrap()
            )
            .context("Failed to read branch created timestamp")?
            .parse::<u128>()
            .context("Failed to parse branch created timestamp")?,
            updated_branch.created_timestamp_ms
        );
        assert_eq!(
            fs::read_to_string(
                root.join("meta")
                    .join("updated_timestamp_ms")
                    .to_str()
                    .unwrap()
            )
            .context("Failed to read branch updated timestamp")?
            .parse::<u128>()
            .context("Failed to parse branch updated timestamp")?,
            updated_branch.updated_timestamp_ms
        );

        Ok(())
    }
}
