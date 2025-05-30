use gitbutler_branch::BranchCreateRequest;

use super::*;

#[test]
fn detect_upstream_commits() {
    let Test { repo, ctx, .. } = &Test::default();

    gitbutler_branch_actions::set_base_branch(
        ctx,
        &"refs/remotes/origin/master".parse().unwrap(),
        false,
        ctx.project().exclusive_worktree_access().write_permission(),
    )
    .unwrap();

    let stack_entry_1 = gitbutler_branch_actions::create_virtual_branch(
        ctx,
        &BranchCreateRequest::default(),
        ctx.project().exclusive_worktree_access().write_permission(),
    )
    .unwrap();

    let oid1 = {
        // create first commit
        fs::write(repo.path().join("file.txt"), "content").unwrap();
        gitbutler_branch_actions::create_commit(ctx, stack_entry_1.id, "commit", None).unwrap()
    };

    let oid2 = {
        // create second commit
        fs::write(repo.path().join("file.txt"), "content2").unwrap();
        gitbutler_branch_actions::create_commit(ctx, stack_entry_1.id, "commit", None).unwrap()
    };

    // push
    gitbutler_branch_actions::stack::push_stack(ctx, stack_entry_1.id, false).unwrap();

    let oid3 = {
        // create third commit
        fs::write(repo.path().join("file.txt"), "content3").unwrap();
        gitbutler_branch_actions::create_commit(ctx, stack_entry_1.id, "commit", None).unwrap()
    };

    {
        // should correctly detect pushed commits
        let list_result = gitbutler_branch_actions::list_virtual_branches(ctx).unwrap();
        let branches = list_result.branches;
        assert_eq!(branches.len(), 1);
        assert_eq!(branches[0].id, stack_entry_1.id);
        assert_eq!(branches[0].series[0].clone().unwrap().patches.len(), 3);
        assert_eq!(branches[0].series[0].clone().unwrap().patches[0].id, oid3);
        assert!(!branches[0].series[0].clone().unwrap().patches[0].is_local_and_remote);
        assert_eq!(branches[0].series[0].clone().unwrap().patches[1].id, oid2);
        assert!(branches[0].series[0].clone().unwrap().patches[1].is_local_and_remote);
        assert_eq!(branches[0].series[0].clone().unwrap().patches[2].id, oid1);
        assert!(branches[0].series[0].clone().unwrap().patches[2].is_local_and_remote);
    }
}

#[test]
fn detect_integrated_commits() {
    let Test { repo, ctx, .. } = &Test::default();

    gitbutler_branch_actions::set_base_branch(
        ctx,
        &"refs/remotes/origin/master".parse().unwrap(),
        false,
        ctx.project().exclusive_worktree_access().write_permission(),
    )
    .unwrap();

    let stack_entry_1 = gitbutler_branch_actions::create_virtual_branch(
        ctx,
        &BranchCreateRequest::default(),
        ctx.project().exclusive_worktree_access().write_permission(),
    )
    .unwrap();

    let oid1 = {
        // create first commit
        fs::write(repo.path().join("file.txt"), "content").unwrap();
        gitbutler_branch_actions::create_commit(ctx, stack_entry_1.id, "commit", None).unwrap()
    };

    let oid2 = {
        // create second commit
        fs::write(repo.path().join("file.txt"), "content2").unwrap();
        gitbutler_branch_actions::create_commit(ctx, stack_entry_1.id, "commit", None).unwrap()
    };

    // push
    gitbutler_branch_actions::stack::push_stack(ctx, stack_entry_1.id, false).unwrap();

    {
        // merge branch upstream
        let branch = gitbutler_branch_actions::list_virtual_branches(ctx)
            .unwrap()
            .branches
            .into_iter()
            .find(|b| b.id == stack_entry_1.id)
            .unwrap();

        let name = branch
            .series
            .first()
            .unwrap()
            .as_ref()
            .unwrap()
            .upstream_reference
            .as_ref()
            .unwrap();
        let refname = Refname::from_str(name).unwrap();

        repo.merge(&refname).unwrap();
        repo.fetch();
    }

    let oid3 = {
        // create third commit
        fs::write(repo.path().join("file.txt"), "content3").unwrap();
        gitbutler_branch_actions::create_commit(ctx, stack_entry_1.id, "commit", None).unwrap()
    };

    {
        // should correctly detect pushed commits
        let list_result = gitbutler_branch_actions::list_virtual_branches(ctx).unwrap();
        let branches = list_result.branches;

        assert_eq!(branches.len(), 1);
        assert_eq!(branches[0].id, stack_entry_1.id);
        assert_eq!(branches[0].series[0].clone().unwrap().patches.len(), 3);
        assert_eq!(branches[0].series[0].clone().unwrap().patches[0].id, oid3);
        assert!(!branches[0].series[0].clone().unwrap().patches[0].is_integrated);
        assert_eq!(branches[0].series[0].clone().unwrap().patches[1].id, oid2);
        assert!(branches[0].series[0].clone().unwrap().patches[1].is_integrated);
        assert_eq!(branches[0].series[0].clone().unwrap().patches[2].id, oid1);
        assert!(branches[0].series[0].clone().unwrap().patches[2].is_integrated);
    }
}
