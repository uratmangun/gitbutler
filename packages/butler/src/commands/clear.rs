use anyhow::{Context, Result};
use clap::Args;

use gblib::{sessions, virtual_branches};

use crate::app::App;

#[derive(Debug, Args)]
pub struct Clear;

impl super::RunCommand for Clear {
    fn run(self) -> Result<()> {
        let app = App::new().context("Failed to create app")?;
        let session = app
            .gb_repository()
            .get_or_create_current_session()
            .context("failed to get or create currnt session")?;
        let gb_repository = app.gb_repository();
        let session_reader = sessions::Reader::open(&gb_repository, &session)
            .context("failed to open current session reader")?;
        let branch_writer = virtual_branches::branch::Writer::new(&gb_repository);

        let iterator =
            virtual_branches::Iterator::new(&session_reader).expect("failed to read branches");
        for branch in iterator.flatten() {
            branch_writer
                .delete(&branch)
                .context("failed to delete branch")?;
        }

        Ok(())
    }
}
