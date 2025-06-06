use std::collections::HashMap;

use anyhow::anyhow;
use but_workspace::{DiffSpec, StackId};
use gitbutler_command_context::CommandContext;
use gitbutler_operating_modes::OperatingMode;
use gitbutler_oplog::{
    OplogExt,
    entry::{OperationKind, SnapshotDetails},
};
use gitbutler_oxidize::OidExt;
use gitbutler_project::access::WorktreeWritePermission;
use gitbutler_stack::VirtualBranchesHandle;

use crate::{Outcome, default_target_setting_if_none, generate::commit_message_blocking};
/// This is a GitButler automation which allows easy handling of uncommitted changes in a repository.
/// At a high level, it will:
///   - Checkout GitButler's workspace branch if not already checked out
///   - Create a new branch if necessary (using a generic canned branch name)
///   - Create a new commit with all uncommitted changes found in the worktree (the request context is used as the commit message)
///
/// Avery time this automation is ran, GitButler will aslo:
///   - Create an oplog snaposhot entry _before_ the automation is executed
///   - Create an oplog snapshot entry _after_ the automation is executed
///   - Create a separate persisted entry recording the request context and IDs for the two oplog snapshots
pub fn handle_changes(
    ctx: &mut CommandContext,
    change_summary: &str,
    external_prompt: Option<String>,
) -> anyhow::Result<Outcome> {
    let mut guard = ctx.project().exclusive_worktree_access();
    let perm = guard.write_permission();

    let vb_state = &VirtualBranchesHandle::new(ctx.project().gb_dir());
    default_target_setting_if_none(ctx, vb_state)?; // Create a default target if none exists.

    let snapshot_before = ctx
        .create_snapshot(
            SnapshotDetails::new(OperationKind::AutoHandleChangesBefore),
            perm,
        )?
        .to_gix();

    let response =
        handle_changes_simple_inner(ctx, change_summary, external_prompt.clone(), vb_state, perm);

    let snapshot_after = ctx
        .create_snapshot(
            SnapshotDetails::new(OperationKind::AutoHandleChangesAfter),
            perm,
        )?
        .to_gix();

    crate::action::persist_action(
        ctx,
        crate::action::ButlerAction::new_mcp(
            crate::ActionHandler::HandleChangesSimple,
            external_prompt,
            change_summary.to_owned(),
            snapshot_before,
            snapshot_after,
            &response,
        ),
    )?;

    response
}

fn handle_changes_simple_inner(
    ctx: &mut CommandContext,
    change_summary: &str,
    external_prompt: Option<String>,
    vb_state: &VirtualBranchesHandle,
    perm: &mut WorktreeWritePermission,
) -> anyhow::Result<Outcome> {
    match gitbutler_operating_modes::operating_mode(ctx) {
        OperatingMode::OpenWorkspace => {
            // No action needed, we're already in the workspace
        }
        OperatingMode::Edit(_) => {
            return Err(anyhow::anyhow!(
                "Cannot handle changes while in edit mode. Please exit edit mode first."
            ));
        }
        OperatingMode::OutsideWorkspace(_) => {
            let default_target = vb_state.get_default_target()?;
            gitbutler_branch_actions::set_base_branch(ctx, &default_target.branch, false, perm)?;
        }
    }

    let repo = ctx.gix_repo()?;

    // Get any assignments that may have been made, which also includes any hunk locks. Assignments should be updated according to locks where applicable.
    let (assignments, _) = but_hunk_assignment::assignments_with_fallback(
        ctx,
        true,
        None::<Vec<but_core::TreeChange>>,
    )
    .map_err(|err| serde_error::Error::new(&*err))?;
    if assignments.is_empty() {
        return Ok(Outcome {
            updated_branches: vec![],
        });
    }

    // Get the current stacks in the workspace, creating one if none exists.
    let stacks = crate::stacks_creating_if_none(ctx, vb_state, &repo, perm)?;

    // Put the assignments into buckets by stack ID.
    let mut stack_assignments: HashMap<StackId, Vec<DiffSpec>> =
        stacks.iter().map(|s| (s.id, vec![])).collect();
    let default_stack_id = stacks
        .first()
        .map(|s| s.id)
        .ok_or_else(|| anyhow::anyhow!("No stacks found in the workspace"))?;
    for assignment in assignments {
        if let Some(stack_id) = assignment.stack_id {
            stack_assignments
                .entry(stack_id)
                .or_default()
                .push(assignment.into());
        } else {
            stack_assignments
                .entry(default_stack_id)
                .or_default()
                .push(assignment.into());
        }
    }
    // Go over the stack_assignments and flatten the diff specs for each stack.
    for (_, specs) in stack_assignments.iter_mut() {
        *specs = but_workspace::flatten_diff_specs(specs.clone());
    }

    let mut updated_branches = vec![];

    let commit_message = if std::env::var("OPENAI_API_KEY").is_ok() {
        // TODO: Provide diff string
        commit_message_blocking(change_summary, &external_prompt.unwrap_or_default(), "")?
    } else {
        change_summary.to_string()
    };

    for (stack_id, diff_specs) in stack_assignments {
        if diff_specs.is_empty() {
            continue;
        }

        let stack_branch_name = stacks
            .iter()
            .find(|s| s.id == stack_id)
            .and_then(|s| s.heads.first().map(|h| h.name.to_string()))
            .ok_or(anyhow!("Could not find associated reference name"))?;

        let outcome = but_workspace::commit_engine::create_commit_simple(
            ctx,
            stack_id,
            None,
            diff_specs,
            commit_message.clone(),
            stack_branch_name.clone(),
            perm,
        )?;

        if let Some(new_commit) = outcome.new_commit {
            updated_branches.push(crate::UpdatedBranch {
                branch_name: stack_branch_name,
                new_commits: vec![new_commit.to_string()],
            });
        }
    }

    Ok(Outcome { updated_branches })
}
