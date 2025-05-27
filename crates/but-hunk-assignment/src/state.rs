/// The name of the file holding our state, useful for watching for changes.
use anyhow::Result;
use gitbutler_command_context::CommandContext;
use serde::{Deserialize, Serialize};

use crate::HunkAssignment;

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct HunkAssignments {
    pub assignments: Vec<HunkAssignment>,
}

pub fn assignments(ctx: &mut CommandContext) -> Result<Vec<HunkAssignment>> {
    let assignments = ctx
        .db()?
        .list_all()?
        .into_iter()
        .map(|a| a.try_into())
        .collect::<Result<Vec<HunkAssignment>>>()?;
    Ok(assignments)
}

pub fn set_assignments(ctx: &mut CommandContext, assignments: Vec<HunkAssignment>) -> Result<()> {
    let assignments: Vec<but_db::models::HunkAssignment> = assignments
        .into_iter()
        .map(|a| a.try_into())
        .collect::<Result<Vec<but_db::models::HunkAssignment>>>()?;
    ctx.db()?.set_all(assignments)
}
