#![allow(clippy::indexing_slicing)]

/// Options for the [`ref_info()`](crate::ref_info) call.
#[derive(Default, Debug, Copy, Clone)]
pub struct Options {
    /// The maximum amount of commits to list *per stack*. Note that a [`StackSegment`](crate::branch::StackSegment) will always have a single commit, if available,
    ///  even if this exhausts the commit limit in that stack.
    /// `0` means the limit is disabled.
    ///
    /// NOTE: Currently, to fetch more commits, make this call again with a higher limit.
    /// Additionally, this is only effective if there is an open-ended graph, for example, when `HEAD` points to `main` with
    /// a lot of commits without a discernible base.
    ///
    /// Callers can check for the limit by looking as the oldest commit - if it has no parents, then the limit wasn't hit, or if it is
    /// connected to a merge-base.
    pub stack_commit_limit: usize,

    /// Perform expensive computations on a per-commit basis.
    ///
    /// Note that less expensive checks are still performed.
    pub expensive_commit_info: bool,
}

pub(crate) mod function {
    use crate::branch::{LocalCommit, LocalCommitRelation, RefLocation, Stack, StackSegment};
    use crate::integrated::{IsCommitIntegrated, MergeBaseCommitGraph};
    use crate::{RefInfo, WorkspaceCommit, branch, is_workspace_ref_name};
    use anyhow::bail;
    use bstr::BString;
    use but_core::ref_metadata::{ValueInfo, Workspace, WorkspaceStack};
    use gitbutler_oxidize::ObjectIdExt as _;
    use gix::prelude::{ObjectIdExt, ReferenceExt};
    use gix::refs::{Category, FullName};
    use gix::revision::walk::Sorting;
    use gix::trace;
    use std::collections::hash_map::Entry;
    use std::collections::{BTreeSet, HashMap, HashSet};
    use tracing::instrument;

    /// Gather information about the current `HEAD` and the workspace that might be associated with it, based on data in `repo` and `meta`.
    /// Use `options` to further configure the call.
    ///
    /// For details, see [`ref_info()`].
    pub fn head_info(
        repo: &gix::Repository,
        meta: &impl but_core::RefMetadata,
        opts: super::Options,
    ) -> anyhow::Result<RefInfo> {
        let head = repo.head()?;
        let existing_ref = match head.kind {
            gix::head::Kind::Unborn(ref_name) => {
                return Ok(RefInfo {
                    workspace_ref_name: None,
                    target_ref: workspace_data_of_workspace_branch(meta, ref_name.as_ref())?
                        .and_then(|ws| ws.target_ref),
                    stacks: vec![Stack {
                        base: None,
                        segments: vec![StackSegment {
                            commits_unique_from_tip: vec![],
                            commits_unique_in_remote_tracking_branch: vec![],
                            remote_tracking_ref_name: None,
                            metadata: branch_metadata_opt(meta, ref_name.as_ref())?,
                            ref_location: Some(RefLocation::OutsideOfWorkspace),
                            ref_name: Some(ref_name),
                        }],
                        stash_status: None,
                    }],
                });
            }
            gix::head::Kind::Detached { .. } => {
                return Ok(RefInfo {
                    workspace_ref_name: None,
                    stacks: vec![],
                    target_ref: None,
                });
            }
            gix::head::Kind::Symbolic(name) => name.attach(repo),
        };
        ref_info(existing_ref, meta, opts)
    }

    /// Gather information about the commit at `existing_ref` and the workspace that might be associated with it,
    /// based on data in `repo` and `meta`.
    ///
    /// Use `options` to further configure the call.
    ///
    /// ### Performance
    ///
    /// Make sure the `repo` is initialized with a decently sized Object cache so querying the same commit multiple times will be cheap(er).
    /// Also, **IMPORTANT**, it must use in-memory objects to avoid leaking objects generated during test-merges to disk!
    #[instrument(level = tracing::Level::DEBUG, skip(meta), err(Debug))]
    pub fn ref_info(
        mut existing_ref: gix::Reference<'_>,
        meta: &impl but_core::RefMetadata,
        opts: super::Options,
    ) -> anyhow::Result<RefInfo> {
        let ws_data = workspace_data_of_workspace_branch(meta, existing_ref.name())?;
        let (workspace_ref_name, target_ref, stored_workspace_stacks) =
            obtain_workspace_info(&existing_ref, meta, ws_data)?;
        let repo = existing_ref.repo;
        // If there are multiple choices for a ref that points to a commit we encounter, use one of these.
        let mut preferred_ref_names = stored_workspace_stacks
            .as_ref()
            .map(|stacks| {
                stacks
                    .iter()
                    .flat_map(|stack| stack.branches.iter().map(|b| b.ref_name.as_ref()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let target_remote_symbolic_name = target_ref
            .as_ref()
            .and_then(|rn| extract_remote_name(rn.as_ref(), &repo.remote_names()));

        let ref_commit = existing_ref.peel_to_commit()?;
        let ref_commit = WorkspaceCommit {
            id: ref_commit.id(),
            inner: ref_commit.decode()?.to_owned(),
        };
        let repo = existing_ref.repo;
        let refs_by_id = collect_refs_by_commit_id(repo)?;
        let target_ref_id = target_ref
            .as_ref()
            .and_then(|rn| try_refname_to_id(repo, rn.as_ref()).transpose())
            .transpose()?;
        let cache = repo.commit_graph_if_enabled()?;
        let mut graph = repo.revision_graph(cache.as_ref());
        let mut boundary = gix::hashtable::HashSet::default();

        let mut stacks = if ref_commit.is_managed() {
            let base: Option<_> = if target_ref_id.is_none() {
                match repo
                    .merge_base_octopus_with_graph(ref_commit.parents.iter().cloned(), &mut graph)
                {
                    Ok(id) => Some(id),
                    Err(err) => {
                        tracing::warn!(
                            "Parents of {existing_ref} are disjoint: {err}",
                            existing_ref = existing_ref.name().as_bstr(),
                        );
                        None
                    }
                }
            } else {
                None
            };
            // The commits we have already associated with a stack segment.
            let mut stacks = Vec::new();
            for commit_id in ref_commit.parents.iter() {
                let tip = *commit_id;
                let base = base
                    .or_else(|| {
                        target_ref_id.and_then(|target_id| {
                            match repo.merge_base_with_graph(target_id, tip, &mut graph) {
                                Ok(id) => Some(id),
                                Err(err) => {
                                    tracing::warn!(
                                        "{existing_ref} and {target_ref} are disjoint: {err}",
                                        existing_ref = existing_ref.name().as_bstr(),
                                        target_ref = target_ref.as_ref().expect(
                                            "target_id is present, must have ref name then"
                                        ),
                                    );
                                    None
                                }
                            }
                        })
                    })
                    .map(|base| base.detach());
                boundary.extend(base);
                let segments = collect_stack_segments(
                    tip.attach(repo),
                    refs_by_id.get(&tip).and_then(|refs| {
                        refs.iter()
                            .find(|rn| preferred_ref_names.iter().any(|orn| *orn == rn.as_ref()))
                            .or_else(|| refs.first())
                            .map(|rn| rn.as_ref())
                    }),
                    Some(RefLocation::ReachableFromWorkspaceCommit),
                    &boundary,
                    &preferred_ref_names,
                    opts.stack_commit_limit,
                    &refs_by_id,
                    meta,
                    target_remote_symbolic_name.as_deref(),
                )?;

                boundary.extend(segments.iter().flat_map(|segment| {
                    segment.commits_unique_from_tip.iter().map(|c| c.id).chain(
                        segment
                            .commits_unique_in_remote_tracking_branch
                            .iter()
                            .map(|c| c.id),
                    )
                }));

                stacks.push(Stack {
                    segments,
                    base,
                    // TODO: but as part of the commits.
                    stash_status: None,
                })
            }
            stacks
        } else {
            if is_workspace_ref_name(existing_ref.name()) {
                // TODO: assure we can recover from that.
                bail!(
                    "Workspace reference {name} didn't point to a managed commit anymore",
                    name = existing_ref.name().shorten()
                )
            }
            // Discover all references that actually point to the reachable graph.
            let tip = ref_commit.id;
            let base = target_ref_id
                .and_then(|target_id| {
                    match repo.merge_base_with_graph(target_id, tip, &mut graph) {
                        Ok(id) => Some(id),
                        Err(err) => {
                            tracing::warn!(
                                "{existing_ref} and {target_ref} are disjoint: {err}",
                                existing_ref = existing_ref.name().as_bstr(),
                                target_ref = target_ref
                                    .as_ref()
                                    .expect("target_id is present, must have ref name then"),
                            );
                            None
                        }
                    }
                })
                .map(|base| base.detach());
            // If we have a workspace, then we have to use that as the basis for our traversal to assure
            // the commits and stacks are assigned consistently.
            if let Some(workspace_ref) = workspace_ref_name
                .as_ref()
                .filter(|workspace_ref| workspace_ref.as_ref() != existing_ref.name())
            {
                let workspace_contains_ref_tip =
                    walk_commits(repo, workspace_ref.as_ref(), base)?.contains(&*tip);
                let workspace_tip_is_managed = try_refname_to_id(repo, workspace_ref.as_ref())?
                    .map(|commit_id| WorkspaceCommit::from_id(commit_id.attach(repo)))
                    .transpose()?
                    .is_some_and(|c| c.is_managed());
                if workspace_contains_ref_tip && workspace_tip_is_managed {
                    // To assure the stack is counted consistently even when queried alone, redo the query.
                    // This should be avoided (i.e., the caller should consume the 'highest value'
                    // refs if possible, but that's not always the case.
                    // TODO(perf): add 'focus' to `opts` so it doesn't do expensive computations for stacks we drop later.
                    let mut info = ref_info(repo.find_reference(workspace_ref)?, meta, opts)?;
                    if let Some((stack_index, segment_index)) = info
                        .stacks
                        .iter()
                        .enumerate()
                        .find_map(|(stack_index, stack)| {
                            stack.segments.iter().enumerate().find_map(
                                |(segment_index, segment)| {
                                    segment
                                        .ref_name
                                        .as_ref()
                                        .is_some_and(|rn| rn.as_ref() == existing_ref.name())
                                        .then_some((stack_index, segment_index))
                                },
                            )
                        })
                    {
                        let mut curr_stack_idx = 0;
                        info.stacks.retain(|_| {
                            let retain = curr_stack_idx == stack_index;
                            curr_stack_idx += 1;
                            retain
                        });
                        let mut curr_segment_idx = 0;
                        info.stacks[0].segments.retain(|_| {
                            let retain = curr_segment_idx >= segment_index;
                            curr_segment_idx += 1;
                            retain
                        });
                    } else {
                        // TODO: a test for that, is it even desirable?
                        info.stacks.clear();
                        trace::warn!(
                            "Didn't find {ref_name} in ref-info, even though commit {tip} is reachable from {workspace_ref}",
                            ref_name = existing_ref.name().as_bstr(),
                        );
                    }
                    return Ok(info);
                }
            }
            let boundary = {
                let mut hs = gix::hashtable::HashSet::default();
                hs.extend(base);
                hs
            };

            preferred_ref_names.push(existing_ref.name());
            let segments = collect_stack_segments(
                tip,
                Some(existing_ref.name()),
                Some(match workspace_ref_name.as_ref().zip(target_ref_id) {
                    None => RefLocation::OutsideOfWorkspace,
                    Some((ws_ref, target_id)) => {
                        let ws_commits = walk_commits(repo, ws_ref.as_ref(), Some(target_id))?;
                        if ws_commits.contains(&*tip) {
                            RefLocation::ReachableFromWorkspaceCommit
                        } else {
                            RefLocation::OutsideOfWorkspace
                        }
                    }
                }),
                &boundary, /* boundary commits */
                &preferred_ref_names,
                opts.stack_commit_limit,
                &refs_by_id,
                meta,
                target_remote_symbolic_name.as_deref(),
            )?;

            vec![Stack {
                // TODO: compute base if target-ref is available, but only if this isn't the target ref!
                base,
                segments,
                stash_status: None,
            }]
        };

        // Various cleanup functions to enforce constraints before spending time on classifying commits.
        enforce_constraints(&mut stacks);
        if let Some(ws_stacks) = stored_workspace_stacks.as_deref() {
            reconcile_with_workspace_stacks(&existing_ref, ws_stacks, &mut stacks, meta)?;
        }

        if opts.expensive_commit_info {
            populate_commit_info(target_ref.as_ref(), &mut stacks, repo, &mut graph)?;
        }

        Ok(RefInfo {
            workspace_ref_name,
            stacks,
            target_ref,
        })
    }

    #[allow(clippy::type_complexity)]
    fn obtain_workspace_info(
        existing_ref: &gix::Reference<'_>,
        meta: &impl but_core::RefMetadata,
        ws_data: Option<Workspace>,
    ) -> anyhow::Result<(
        Option<FullName>,
        Option<FullName>,
        Option<Vec<WorkspaceStack>>,
    )> {
        Ok(if let Some(ws_data) = ws_data {
            (
                Some(existing_ref.name().to_owned()),
                ws_data.target_ref,
                Some(ws_data.stacks),
            )
        } else {
            // We'd want to assure we don't overcount commits even if we are handed a non-workspace ref, so we always have to
            // search for known workspaces.
            // Do get the first known target ref for now.
            let ws_data_iter = meta
                .iter()
                .filter_map(Result::ok)
                .filter_map(|(ref_name, item)| {
                    item.downcast::<but_core::ref_metadata::Workspace>()
                        .ok()
                        .map(|ws| (ref_name, ws))
                });
            let mut target_refs =
                ws_data_iter.map(|(ref_name, ws)| (ref_name, ws.target_ref, ws.stacks));
            let first_target = target_refs.next();
            if target_refs.next().is_some() {
                bail!(
                    "BUG: found more than one workspaces in branch-metadata, and we'd want to make this code multi-workspace compatible"
                )
            }
            first_target
                .map(|(a, b, c)| (Some(a), b, Some(c)))
                .unwrap_or_default()
        })
    }

    /// Does the following:
    ///
    /// * a segment can be reachable from multiple stacks. If a segment is also a stack, remove it along with all segments
    ///   that follow as one can assume they are contained in the stack.
    fn enforce_constraints(stacks: &mut [Stack]) {
        let mut for_deletion = Vec::new();
        for (stack_idx, stack_name) in stacks
            .iter()
            .enumerate()
            .filter_map(|(idx, stack)| stack.name().map(|n| (idx, n)))
        {
            for (other_stack_idx, other_stack) in stacks
                .iter()
                .enumerate()
                .filter(|(idx, _stack)| *idx != stack_idx)
            {
                if let Some(matching_segment_idx) = other_stack
                    .segments
                    .iter()
                    .enumerate()
                    .find_map(|(idx, segment)| {
                        (segment.ref_name.as_ref().map(|rn| rn.as_ref()) == Some(stack_name))
                            .then_some(idx)
                    })
                {
                    for_deletion.push((other_stack_idx, matching_segment_idx));
                }
            }
        }

        for (stack_idx, first_segment_idx_to_delete) in for_deletion {
            stacks[stack_idx]
                .segments
                .drain(first_segment_idx_to_delete..);
        }
    }

    /// Given the desired stack configuration in `ws_stacks`, bring this information into `stacks` which is assumed
    /// to be what Git really sees.
    /// This is needed as empty stacks and segments, and particularly their order, can't be fully represented just
    /// with Git refs alone.
    fn reconcile_with_workspace_stacks(
        existing_ref: &gix::Reference<'_>,
        ws_stacks: &[WorkspaceStack],
        stacks: &mut Vec<Stack>,
        meta: &impl but_core::RefMetadata,
    ) -> anyhow::Result<()> {
        validate_workspace_stacks(ws_stacks)?;
        // Stacks that are genuinely reachable we have to show.
        // Empty ones are special as they don't have their own commits and aren't distinguishable by traversing a
        // workspace commit. For this, we have workspace metadata to tell us what is what.
        // The goal is to remove segments that we found by traversal and re-add them as individual stack if they are some.
        // TODO: remove this block once that is folded into the function below.
        let mut stack_idx_to_remove = Vec::new();
        for (idx, stack) in stacks
            .iter_mut()
            .enumerate()
            .filter(|(_, stack)| stack.name() != Some(existing_ref.name()))
        {
            // Find all empty segments that aren't listed in our workspace stacks metadata, and remove them.
            let desired_stack_segments = ws_stacks.iter().find(|ws_stack| {
                ws_stack
                    .branches
                    .first()
                    .is_some_and(|branch| Some(branch.ref_name.as_ref()) == stack.name())
            });
            let num_segments_to_keep = stack
                .segments
                .iter()
                .enumerate()
                .rev()
                .by_ref()
                .take_while(|(_idx, segment)| segment.commits_unique_from_tip.is_empty())
                .take_while(|(_idx, segment)| {
                    segment
                        .ref_name
                        .as_ref()
                        .zip(desired_stack_segments)
                        .is_none_or(|(srn, desired_stack)| {
                            // We don't let the desired order matter, just that an empty segment is (not) mentioned.
                            desired_stack
                                .branches
                                .iter()
                                .all(|branch| &branch.ref_name != srn)
                        })
                })
                .map(|t| t.0)
                .last();
            if let Some(keep) = num_segments_to_keep {
                stack.segments.drain(keep..);
            }

            if stack.segments.is_empty() {
                stack_idx_to_remove.push(idx);
            }
        }
        if !stack_idx_to_remove.is_empty() {
            let mut idx = 0;
            stacks.retain(|_stack| {
                let res = !stack_idx_to_remove.contains(&idx);
                idx += 1;
                res
            });
        }

        // Put the stacks into the right order, and create empty stacks for those that are completely virtual.
        sort_stacks_by_order_in_ws_stacks(existing_ref.repo, stacks, ws_stacks, meta)?;
        Ok(())
    }

    /// Basic validation for virtual workspaces that our processing builds upon.
    fn validate_workspace_stacks(stacks: &[WorkspaceStack]) -> anyhow::Result<()> {
        let mut seen = BTreeSet::new();
        for name in stacks
            .iter()
            .flat_map(|stack| stack.branches.iter().map(|branch| branch.ref_name.as_ref()))
        {
            let first = seen.insert(name);
            if !first {
                bail!(
                    "invalid workspace stack: duplicate ref name: {}",
                    name.as_bstr()
                )
            }
        }
        Ok(())
    }

    /// Brute-force insert missing stacks and segments as determined in `ordered`.
    /// Add missing stacks and segments as well so real stacks `unordered` match virtual stacks `ordered`.
    /// Note that `ordered` is assumed to be validated.
    fn sort_stacks_by_order_in_ws_stacks(
        repo: &gix::Repository,
        unordered: &mut Vec<Stack>,
        ordered: &[WorkspaceStack],
        meta: &impl but_core::RefMetadata,
    ) -> anyhow::Result<()> {
        // With this serialized set of desired segments, the tip can also be missing, and we still have
        // a somewhat expected order. Besides, it's easier to work with.
        // We also only include those that exist as ref (which is always a requirement now).
        let serialized_virtual_segments = {
            let mut v = Vec::new();
            for (is_stack_tip, existing_ws_ref) in ordered.iter().flat_map(|ws_stack| {
                ws_stack
                    .branches
                    .iter()
                    .enumerate()
                    .filter_map(|(branch_idx, branch)| {
                        repo.try_find_reference(branch.ref_name.as_ref())
                            .transpose()
                            .map(|rn| (branch_idx == 0, rn))
                    })
            }) {
                let mut existing_ws_ref = existing_ws_ref?;
                let id = existing_ws_ref.peel_to_id_in_place()?.detach();
                v.push((is_stack_tip, id, existing_ws_ref.inner.name));
            }
            v
        };

        let existing_virtual_stacks = serialized_virtual_segments.iter().enumerate().filter_map(
            |(segment_idx, (is_stack_tip, id, segment_ref_name))| {
                if !is_stack_tip {
                    return None;
                }
                let stack_tip = (*id, segment_ref_name);
                let segments = serialized_virtual_segments
                    .get(segment_idx + 1..)
                    .map(|slice| {
                        slice
                            .iter()
                            .take_while(|(is_stack, _, _)| !is_stack)
                            .map(|(_, id, rn)| (*id, rn))
                    })
                    .into_iter()
                    .flatten();
                Some(Some(stack_tip).into_iter().chain(segments))
            },
        );

        // Identify missing (existing) stacks in ordered and add them to the end of unordered.
        // Here we must match only the existing stack-heads.
        for virtual_segments in existing_virtual_stacks {
            // Find a real stack where one segment intersects with the desired stacks to know what that work on.
            let virtual_stack_is_known_as_real_stack = unordered.iter_mut().find(|stack| {
                stack.segments.iter().any(|s| {
                    virtual_segments
                        .clone()
                        .any(|(_, vs_name)| s.ref_name.as_ref() == Some(vs_name))
                })
            });
            if let Some(real_stack) = virtual_stack_is_known_as_real_stack {
                // We know there is one virtual ref name bonding the real stack with the virtual one.
                // Now all we have to do is to place the non-existing virtual segments into the right position
                // alongside their segments. It's notable that these virtual segments can be placed in any position.
                // They may also already exist, and may be stacked, so multiple empty ones are on top of each other.
                // At its core, we want to consume N consecutive segments and insert them into position X.
                // BUT ALSO NOTE: We cannot reorder real segments that are 'locked' to the real commit-graph, nor can
                //                we reorder real segments that are found at a certain commit, but empty.
                // TODO: also delete empty real segments

                let find_base =
                    |start_idx: usize, segments: &[StackSegment]| -> Option<gix::ObjectId> {
                        segments.get(start_idx..).and_then(|slice| {
                            slice.iter().find_map(|segment| {
                                segment.commits_unique_from_tip.first().map(|c| c.id)
                            })
                        })
                    };
                let mut insert_position = 0;
                for (target_id, virtual_segment_ref_name) in virtual_segments {
                    let real_stack_idx =
                        real_stack
                            .segments
                            .iter()
                            .enumerate()
                            .find_map(|(idx, real_segment)| {
                                (real_segment.ref_name.as_ref() == Some(virtual_segment_ref_name))
                                    .then_some(idx)
                            });
                    match real_stack_idx {
                        None => {
                            if let Some(mismatched_base) =
                                find_base(insert_position, &real_stack.segments)
                                    .filter(|base| *base != target_id)
                            {
                                tracing::warn!(
                                    "Somehow virtual ref '{name}' was supposed to be at {}, but its closest insertion base was {}",
                                    target_id,
                                    mismatched_base,
                                    name = virtual_segment_ref_name.as_bstr(),
                                );
                                continue;
                            }
                            real_stack.segments.insert(
                                insert_position,
                                segment_from_ref_name(
                                    repo,
                                    meta,
                                    virtual_segment_ref_name.as_ref(),
                                )?,
                            );
                            insert_position += 1;
                        }
                        Some(existing_idx) => {
                            if real_stack.segments[existing_idx]
                                .commits_unique_from_tip
                                .is_empty()
                            {
                                // TODO: do assure empty segments (despite real) are correctly sorted, and we can re-sort these
                            }
                            // Skip this one, it's already present
                            insert_position = existing_idx + 1;
                        }
                    }
                }
            } else {
                // We have a virtual stack that wasn't reachable in reality at all.
                // Add it as a separate stack then, reproducing each segment verbatim.
                // TODO: we actually have to assure that the recorded order still is compatible with the
                //       associated real-world IDs. This should be reconciled before using the virtual segments!!
                let mut segments = Vec::new();
                let mut last_seen_target_id_as_base = None;
                for (target_id, segment_ref_name) in virtual_segments {
                    last_seen_target_id_as_base = Some(target_id);
                    segments.push(segment_from_ref_name(
                        repo,
                        meta,
                        segment_ref_name.as_ref(),
                    )?);
                }
                // From this segment
                unordered.push(Stack {
                    base: last_seen_target_id_as_base,
                    segments,
                    // TODO: set up
                    stash_status: None,
                });
            }
        }

        // Sort existing, and put those that aren't matched to the top as they are usually traversed,
        // and 'more real'.
        unordered.sort_by(|a, b| {
            let index_a = serialized_virtual_segments
                .iter()
                .enumerate()
                .find_map(|(idx, (_, _, segment_ref_name))| {
                    (Some(segment_ref_name) == a.ref_name()).then_some(idx)
                })
                .unwrap_or_default();
            let index_b = serialized_virtual_segments
                .iter()
                .enumerate()
                .find_map(|(idx, (_, _, segment_ref_name))| {
                    (Some(segment_ref_name) == b.ref_name()).then_some(idx)
                })
                .unwrap_or_default();
            index_a.cmp(&index_b)
        });
        // TODO: integrate segments into existing stacks.

        // TODO: log all stack segments that couldn't be matched, even though we should probably do something
        //       with them eventually.
        Ok(())
    }

    fn segment_from_ref_name(
        repo: &gix::Repository,
        meta: &impl but_core::RefMetadata,
        virtual_segment_ref_name: &gix::refs::FullNameRef,
    ) -> anyhow::Result<StackSegment> {
        Ok(StackSegment {
            ref_name: Some(virtual_segment_ref_name.to_owned()),
            remote_tracking_ref_name: lookup_remote_tracking_branch(
                repo,
                virtual_segment_ref_name,
            )?,
            // TODO: this isn't important yet, but it's probably also not always correct.
            ref_location: Some(RefLocation::ReachableFromWorkspaceCommit),
            // Always empty, otherwise we would have found the segment by traversal.
            commits_unique_from_tip: vec![],
            // Will be set when expensive data is computed.
            commits_unique_in_remote_tracking_branch: vec![],
            metadata: meta
                .branch_opt(virtual_segment_ref_name)?
                .map(|b| b.clone()),
        })
    }

    /// Akin to `log()`, but less powerful.
    // TODO: replace with something better, and also use `.hide()`.
    fn walk_commits(
        repo: &gix::Repository,
        from: &gix::refs::FullNameRef,
        hide: Option<gix::ObjectId>,
    ) -> anyhow::Result<gix::hashtable::HashSet<gix::ObjectId>> {
        let Some(from_id) = repo
            .try_find_reference(from)?
            .and_then(|mut r| r.peel_to_id_in_place().ok())
        else {
            return Ok(Default::default());
        };
        Ok(from_id
            .ancestors()
            .sorting(Sorting::BreadthFirst)
            // TODO: use 'hide()'
            .with_boundary(hide)
            .all()?
            .filter_map(Result::ok)
            .map(|info| info.id)
            .collect())
    }

    fn lookup_remote_tracking_branch(
        repo: &gix::Repository,
        ref_name: &gix::refs::FullNameRef,
    ) -> anyhow::Result<Option<gix::refs::FullName>> {
        Ok(repo
            .branch_remote_tracking_ref_name(ref_name, gix::remote::Direction::Fetch)
            .transpose()?
            .map(|rn| rn.into_owned()))
    }

    fn lookup_remote_tracking_branch_or_deduce_it(
        repo: &gix::Repository,
        ref_name: &gix::refs::FullNameRef,
        symbolic_remote_name: Option<&str>,
    ) -> anyhow::Result<Option<gix::refs::FullName>> {
        Ok(lookup_remote_tracking_branch(repo, ref_name)?.or_else(|| {
            let symbolic_remote_name = symbolic_remote_name?;
            // Deduce the ref-name as fallback.
            // TODO: remove this - this is only required to support legacy repos that
            //       didn't setup normal Git remotes.
            // let remote_name = target_
            let remote_tracking_ref_name = format!(
                "refs/remotes/{symbolic_remote_name}/{short_name}",
                short_name = ref_name.shorten()
            );
            repo.find_reference(&remote_tracking_ref_name)
                .ok()
                .map(|remote_ref| remote_ref.name().to_owned())
        }))
    }

    fn extract_remote_name(
        ref_name: &gix::refs::FullNameRef,
        remotes: &gix::remote::Names<'_>,
    ) -> Option<String> {
        let (category, shorthand_name) = ref_name.category_and_short_name()?;
        if !matches!(category, Category::RemoteBranch) {
            return None;
        }

        let longest_remote = remotes
            .iter()
            .rfind(|reference_name| shorthand_name.starts_with(reference_name))
            .ok_or(anyhow::anyhow!(
                "Failed to find remote branch's corresponding remote"
            ))
            .ok()?;
        Some(longest_remote.to_string())
    }

    /// For each stack in `stacks`, and for each stack segment within it, check if a remote tracking branch is available
    /// and existing. Then find its commits and fill in commit-information of the commits that are reachable by the stack tips as well.
    ///
    /// `graph` is used to speed up merge-base queries.
    ///
    /// **IMPORTANT**: `repo` must use in-memory objects!
    /// TODO: have merge-graph based checks that can check if one commit is included in the ancestry of another tip. That way one can
    ///       quick perform is-integrated checks with the target branch.
    fn populate_commit_info<'repo>(
        target_ref_name: Option<&gix::refs::FullName>,
        stacks: &mut [Stack],
        repo: &'repo gix::Repository,
        merge_graph: &mut MergeBaseCommitGraph<'repo, '_>,
    ) -> anyhow::Result<()> {
        #[derive(Hash, Clone, Eq, PartialEq)]
        enum ChangeIdOrCommitData {
            ChangeId(String),
            CommitData {
                author: gix::actor::Signature,
                message: BString,
            },
        }
        let mut boundary = gix::hashtable::HashSet::default();
        let mut ambiguous_commits = HashSet::<ChangeIdOrCommitData>::new();
        // NOTE: The check for similarity is currently run across all remote branches in the stack.
        //       Further, this doesn't handle reorderings/topology differences at all, it's just there or not.
        let mut similarity_lut = HashMap::<ChangeIdOrCommitData, gix::ObjectId>::new();
        let git2_repo = git2::Repository::open(repo.path())?;
        for stack in stacks {
            boundary.clear();
            boundary.extend(stack.base);

            let segments_with_remote_ref_tips_and_base: Vec<_> = stack
                .segments
                .iter()
                .enumerate()
                .map(|(index, segment)| {
                    let remote_ref_tip =
                        segment
                            .remote_tracking_ref_name
                            .as_ref()
                            .and_then(|remote_ref_name| {
                                try_refname_to_id(repo, remote_ref_name.as_ref())
                                    .ok()
                                    .flatten()
                            });
                    (index, remote_ref_tip)
                })
                .collect();
            // Start the remote commit collection on the segment with the first remote,
            // and stop commit-status handling at the first segment which has a remote (as it would be a new starting point).
            let segments_with_remote_ref_tips_and_base: Vec<_> =
                segments_with_remote_ref_tips_and_base
                    .iter()
                    // TODO: a test for this: remote_ref_tip selects the start, and the base is always the next start's tip or the stack base.
                    .map(|(index, remote_ref_tip)| {
                        let remote_ref_tip_and_base = remote_ref_tip.and_then(|remote_ref_tip| {
                            segments_with_remote_ref_tips_and_base
                                .get((index + 1)..)
                                .and_then(|slice| {
                                    slice.iter().find_map(|(index, remote_ref_tip)| {
                                        remote_ref_tip.and_then(|_| stack.segments[*index].tip())
                                    })
                                })
                                .or(stack.base)
                                .map(|base| (remote_ref_tip, base))
                        });
                        (index, remote_ref_tip_and_base)
                    })
                    .collect();

            for (segment_index, remote_ref_tip_and_base) in segments_with_remote_ref_tips_and_base {
                let segment = &mut stack.segments[*segment_index];
                if let Some((remote_ref_tip, base_for_remote)) = remote_ref_tip_and_base {
                    boundary.insert(base_for_remote);

                    let mut insert_or_expell_ambiguous =
                        |k: ChangeIdOrCommitData, v: gix::ObjectId| {
                            if ambiguous_commits.contains(&k) {
                                return;
                            }
                            match similarity_lut.entry(k) {
                                Entry::Occupied(ambiguous) => {
                                    ambiguous_commits.insert(ambiguous.key().clone());
                                    ambiguous.remove();
                                }
                                Entry::Vacant(entry) => {
                                    entry.insert(v);
                                }
                            }
                        };

                    'remote_branch_traversal: for info in remote_ref_tip
                        .attach(repo)
                        .ancestors()
                        .first_parent_only()
                        .sorting(Sorting::BreadthFirst)
                        // TODO: boundary should be 'hide'.
                        .selected(|commit_id_to_yield| !boundary.contains(commit_id_to_yield))?
                    {
                        let info = info?;
                        // Don't break, maybe the local commits are reachable through multiple avenues.
                        if let Some(idx) = segment
                            .commits_unique_from_tip
                            .iter_mut()
                            .enumerate()
                            .find_map(|(idx, c)| (c.id == info.id).then_some(idx))
                        {
                            // Mark all commits from here as pushed.
                            for commit in &mut segment.commits_unique_from_tip[idx..] {
                                commit.relation = LocalCommitRelation::LocalAndRemote(commit.id);
                            }
                            break 'remote_branch_traversal;
                        } else {
                            let commit = but_core::Commit::from_id(info.id())?;
                            let has_conflicts = commit.is_conflicted();
                            if let Some(hdr) = commit.headers() {
                                insert_or_expell_ambiguous(
                                    ChangeIdOrCommitData::ChangeId(hdr.change_id),
                                    commit.id.detach(),
                                );
                            }
                            insert_or_expell_ambiguous(
                                ChangeIdOrCommitData::CommitData {
                                    author: commit.author.clone(),
                                    message: commit.message.clone(),
                                },
                                commit.id.detach(),
                            );
                            segment.commits_unique_in_remote_tracking_branch.push(
                                branch::RemoteCommit {
                                    inner: commit.into(),
                                    has_conflicts,
                                },
                            );
                        }
                    }
                }

                // Find duplicates harder by change-ids by commit-data.
                for local_commit in &mut segment.commits_unique_from_tip {
                    let commit = but_core::Commit::from_id(local_commit.id.attach(repo))?;
                    if let Some(remote_commit_id) = commit
                        .headers()
                        .and_then(|hdr| {
                            similarity_lut.get(&ChangeIdOrCommitData::ChangeId(hdr.change_id))
                        })
                        .or_else(|| {
                            similarity_lut.get(&ChangeIdOrCommitData::CommitData {
                                author: commit.author.clone(),
                                message: commit.message.clone(),
                            })
                        })
                    {
                        local_commit.relation =
                            LocalCommitRelation::LocalAndRemote(*remote_commit_id);
                    }
                    local_commit.has_conflicts = commit.is_conflicted();
                }

                // Prune upstream commits so they don't show if they are considered locally available as well.
                // This is kind of 'wrong', and we can hope that code doesn't rely on upstream commits.
                segment
                    .commits_unique_in_remote_tracking_branch
                    .retain(|remote_commit| {
                        let remote_commit_is_shared_in_local = segment
                            .commits_unique_from_tip
                            .iter()
                            .any(|c| matches!(c.relation,  LocalCommitRelation::LocalAndRemote(rid) if rid == remote_commit.id));
                        !remote_commit_is_shared_in_local
                    });
            }

            // Finally, check for integration into the target if available.
            // TODO: This can probably be more efficient if this is staged, by first trying
            //       to check if the tip is merged, to flag everything else as merged.
            let mut is_integrated = false;
            if let Some(target_ref_name) = target_ref_name {
                let mut check_commit = IsCommitIntegrated::new2(
                    repo,
                    &git2_repo,
                    target_ref_name.as_ref(),
                    merge_graph,
                )?;
                // TODO: remote commits could also be integrated, this seems overly simplified.
                // For now, just emulate the current implementation (hopefully).
                for local_commit in stack
                    .segments
                    .iter_mut()
                    .flat_map(|segment| &mut segment.commits_unique_from_tip)
                {
                    if is_integrated || {
                        let commit = git2_repo.find_commit(local_commit.id.to_git2())?;
                        check_commit.is_integrated(&commit)
                    }? {
                        is_integrated = true;
                        local_commit.relation = LocalCommitRelation::Integrated;
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn try_refname_to_id(
        repo: &gix::Repository,
        refname: &gix::refs::FullNameRef,
    ) -> anyhow::Result<Option<gix::ObjectId>> {
        Ok(repo
            .try_find_reference(refname)?
            .map(|mut r| r.peel_to_id_in_place())
            .transpose()?
            .map(|id| id.detach()))
    }

    /// Walk down the commit-graph from `tip` until a `boundary_commits` is encountered, excluding it, or to the graph root if there is no boundary.
    /// Walk along the first parent, and return stack segments on its path using the `refs_by_commit_id` reverse mapping in walk order.
    /// `tip_ref` is the name of the reference pointing to `tip` if it's known.
    /// `ref_location` it the location of `tip_ref`
    /// `preferred_refs` is an arbitrarily sorted array of names that should be used in the returned segments if they are encountered during the traversal
    /// *and* there are more than one ref pointing to it.
    /// `symbolic_remote_name` is used to infer the name of the remote tracking ref in case `tip_ref` doesn't have a remote configured.
    ///
    /// Note that `boundary_commits` are sorted so binary-search can be used to quickly check membership.
    ///
    /// ### Important
    ///
    /// This function does *not* fill in remote information *nor* does it compute the per-commit status.
    /// TODO: also add `hidden` commits, for a list of special commits like the merge-base where all parents should be hidden as well.
    ///       Right now we are completely relying on (many) boundary commits which should work most of the time, but may not work if
    ///       branches have diverged a lot.
    #[allow(clippy::too_many_arguments)]
    fn collect_stack_segments(
        tip: gix::Id<'_>,
        tip_ref: Option<&gix::refs::FullNameRef>,
        ref_location: Option<RefLocation>,
        boundary_commits: &gix::hashtable::HashSet,
        preferred_refs: &[&gix::refs::FullNameRef],
        mut limit: usize,
        refs_by_id: &RefsById,
        meta: &impl but_core::RefMetadata,
        symbolic_remote_name: Option<&str>,
    ) -> anyhow::Result<Vec<StackSegment>> {
        let mut out = Vec::new();
        let mut segment = Some(StackSegment {
            ref_name: tip_ref.map(ToOwned::to_owned),
            ref_location,
            // the tip is part of the walk.
            ..Default::default()
        });
        for (count, info) in tip
            .ancestors()
            .first_parent_only()
            .sorting(Sorting::BreadthFirst)
            // TODO: boundary should be 'hide'.
            .selected(|id_to_yield| !boundary_commits.contains(id_to_yield))?
            .enumerate()
        {
            let segment_ref = segment.as_mut().expect("a segment is always present here");

            if limit != 0 && count >= limit {
                if segment_ref.commits_unique_from_tip.is_empty() {
                    limit += 1;
                } else {
                    out.extend(segment.take());
                    break;
                }
            }
            let info = info?;
            if let Some(refs) = refs_by_id.get(&info.id) {
                let ref_at_commit = refs
                    .iter()
                    .find(|rn| preferred_refs.iter().any(|orn| *orn == rn.as_ref()))
                    .or_else(|| refs.first())
                    .map(|rn| rn.to_owned());
                if ref_at_commit.as_ref().map(|rn| rn.as_ref()) == tip_ref {
                    segment_ref
                        .commits_unique_from_tip
                        .push(LocalCommit::new_from_id(info.id())?);
                    continue;
                }
                out.extend(segment);
                segment = Some(StackSegment {
                    ref_name: ref_at_commit,
                    ref_location,
                    commits_unique_from_tip: vec![LocalCommit::new_from_id(info.id())?],
                    commits_unique_in_remote_tracking_branch: vec![],
                    // The fields that follow will be set later.
                    remote_tracking_ref_name: None,
                    metadata: None,
                });
                continue;
            } else {
                segment_ref
                    .commits_unique_from_tip
                    .push(LocalCommit::new_from_id(info.id())?);
            }
        }
        out.extend(segment);

        let repo = tip.repo;
        for segment in out.iter_mut() {
            let Some(ref_name) = segment.ref_name.as_ref() else {
                continue;
            };
            segment.remote_tracking_ref_name = lookup_remote_tracking_branch_or_deduce_it(
                repo,
                ref_name.as_ref(),
                symbolic_remote_name,
            )?;
            let branch_info = meta.branch(ref_name.as_ref())?;
            if !branch_info.is_default() {
                segment.metadata = Some((*branch_info).clone())
            }
        }
        Ok(out)
    }

    // A trait of the ref-names array is that these are sorted, as they are from a sorted traversal, giving us stable ordering.
    type RefsById = gix::hashtable::HashMap<gix::ObjectId, Vec<gix::refs::FullName>>;

    // Create a mapping of all heads to the object ids they point to.
    // No tags are used (yet), but maybe that's useful in the future.
    // We never pick up branches we consider to be part of the workspace.
    fn collect_refs_by_commit_id(repo: &gix::Repository) -> anyhow::Result<RefsById> {
        let mut all_refs_by_id = gix::hashtable::HashMap::<_, Vec<_>>::default();
        for (commit_id, git_reference) in repo
            .references()?
            .prefixed("refs/heads/")?
            .filter_map(Result::ok)
            .filter_map(|r| {
                if is_workspace_ref_name(r.name()) {
                    return None;
                }
                r.try_id().map(|id| (id.detach(), r.inner.name))
            })
        {
            all_refs_by_id
                .entry(commit_id)
                .or_default()
                .push(git_reference);
        }
        all_refs_by_id.values_mut().for_each(|v| v.sort());
        Ok(all_refs_by_id)
    }

    // TODO: Put this in `RefMetadataExt` if useful elsewhere.
    fn branch_metadata_opt(
        meta: &impl but_core::RefMetadata,
        name: &gix::refs::FullNameRef,
    ) -> anyhow::Result<Option<but_core::ref_metadata::Branch>> {
        let md = meta.branch(name)?;
        Ok(if md.is_default() {
            None
        } else {
            Some((*md).clone())
        })
    }

    // Fetch non-default workspace information, but only if reference at `name` seems to be a workspace reference.
    pub fn workspace_data_of_workspace_branch(
        meta: &impl but_core::RefMetadata,
        name: &gix::refs::FullNameRef,
    ) -> anyhow::Result<Option<but_core::ref_metadata::Workspace>> {
        if !is_workspace_ref_name(name) {
            return Ok(None);
        }

        let md = meta.workspace(name)?;
        Ok(if md.is_default() {
            None
        } else {
            Some((*md).clone())
        })
    }

    /// Like [`workspace_data_of_workspace_branch()`], but it will try the name of the default GitButler workspace branch.
    pub fn workspace_data_of_default_workspace_branch(
        meta: &impl but_core::RefMetadata,
    ) -> anyhow::Result<Option<but_core::ref_metadata::Workspace>> {
        workspace_data_of_workspace_branch(
            meta,
            "refs/heads/gitbutler/workspace"
                .try_into()
                .expect("statically known"),
        )
    }
}
