use but_hunk_assignment::HunkAssignmentRequest;
use but_workspace::StackId;
use gitbutler_command_context::CommandContext;

use crate::command;

pub(crate) fn assign_file_to_branch(
    ctx: &mut CommandContext,
    path: &str,
    branch_name: &str,
) -> anyhow::Result<()> {
    let reqs = to_assignment_request(ctx, path, Some(branch_name))?;
    do_assignments(ctx, reqs)
}

pub(crate) fn unassign_file(ctx: &mut CommandContext, path: &str) -> anyhow::Result<()> {
    let reqs = to_assignment_request(ctx, path, None)?;
    do_assignments(ctx, reqs)
}

pub(crate) fn assign_all(
    ctx: &mut CommandContext,
    from_branch: Option<&str>,
    to_branch: Option<&str>,
) -> anyhow::Result<()> {
    let from_stack_id = branch_name_to_stack_id(ctx, from_branch)?;
    let to_stack_id = branch_name_to_stack_id(ctx, to_branch)?;

    // Get all assignment requests from the from_stack_id
    let changes =
        but_core::diff::ui::worktree_changes_by_worktree_dir(ctx.project().path.clone())?.changes;
    let (assignments, _assignments_error) =
        but_hunk_assignment::assignments_with_fallback(ctx, false, Some(changes.clone()))?;

    let mut reqs = Vec::new();
    for assignment in assignments {
        if assignment.stack_id == from_stack_id {
            reqs.push(HunkAssignmentRequest {
                hunk_header: assignment.hunk_header,
                path_bytes: assignment.path_bytes,
                stack_id: to_stack_id,
            });
        }
    }
    do_assignments(ctx, reqs)
}

fn do_assignments(
    ctx: &mut CommandContext,
    reqs: Vec<HunkAssignmentRequest>,
) -> anyhow::Result<()> {
    let rejections = but_hunk_assignment::assign(ctx, reqs)?;
    if !rejections.is_empty() {
        command::print(&rejections, false)?;
    }
    Ok(())
}

fn branch_name_to_stack_id(
    ctx: &CommandContext,
    branch_name: Option<&str>,
) -> anyhow::Result<Option<StackId>> {
    let stack_id = if let Some(branch_name) = branch_name {
        crate::log::stacks(ctx)?
            .iter()
            .find(|s| s.heads.iter().any(|h| h.name == branch_name))
            .map(|s| s.id)
    } else {
        None
    };
    Ok(stack_id)
}

fn to_assignment_request(
    ctx: &mut CommandContext,
    path: &str,
    branch_name: Option<&str>,
) -> anyhow::Result<Vec<HunkAssignmentRequest>> {
    let stack_id = branch_name_to_stack_id(ctx, branch_name)?;

    let changes =
        but_core::diff::ui::worktree_changes_by_worktree_dir(ctx.project().path.clone())?.changes;
    let (assignments, _assignments_error) =
        but_hunk_assignment::assignments_with_fallback(ctx, false, Some(changes.clone()))?;
    let mut reqs = Vec::new();
    for assignment in assignments {
        if assignment.path == path {
            reqs.push(HunkAssignmentRequest {
                hunk_header: assignment.hunk_header,
                path_bytes: assignment.path_bytes,
                stack_id,
            });
        }
    }
    Ok(reqs)
}
