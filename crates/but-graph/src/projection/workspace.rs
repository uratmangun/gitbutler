use std::{
    cell::RefCell,
    collections::{BTreeSet, VecDeque},
    fmt::Formatter,
};

use anyhow::Context;
use but_core::ref_metadata;
use gix::reference::Category;
use petgraph::{Direction, prelude::EdgeRef, visit::NodeRef};
use tracing::instrument;

use crate::{
    CommitFlags, Graph, Segment, SegmentIndex,
    projection::{Stack, StackCommit, StackCommitFlags, StackSegment},
};

/// A workspace is a list of [Stacks](Stack).
#[derive(Clone)]
pub struct Workspace<'graph> {
    /// The underlying graph for providing simplified access to data.
    pub graph: &'graph Graph,
    /// An ID which uniquely identifies the [graph segment](Segment) that represents the tip of the workspace.
    pub id: SegmentIndex,
    /// Specify what kind of workspace this is.
    pub kind: WorkspaceKind,
    /// One or more stacks that live in the workspace.
    pub stacks: Vec<Stack>,
    /// The bound can be imagined as the commit from which all other commits in the workspace originate.
    /// It can also be imagined to be the delimiter at the bottom beyond which nothing belongs to the workspace,
    /// as antagonist to the first commit in tip of the segment with `id`, serving as first commit that is
    /// inside the workspace.
    ///
    /// As such, it's always the longest path to the first shared commit with the target among
    /// all of our stacks, or it is the first commit that is shared among all of our stacks in absence of a target.
    /// One can also think of it as the starting point from which all workspace commits can be reached when
    /// following all incoming connections and stopping at the tip of the workspace.
    ///
    /// It is `None` there is only a single stack and no target, so nothing was integrated.
    pub lower_bound: Option<gix::ObjectId>,
    /// If `base` is set, this is the segment owning the commit.
    pub lower_bound_segment_id: Option<SegmentIndex>,
    /// The target to integrate workspace stacks into.
    ///
    /// If `None`, this is a local workspace that doesn't know when possibly pushed branches are considered integrated.
    /// This happens when there is a local branch checked out without a remote tracking branch.
    pub target: Option<Target>,
    /// The segment index of the extra target as provided for traversal,
    /// useful for AdHoc workspaces, but generally applicable to all workspaces to keep the lower bound lower than it
    /// otherwise would be.
    pub extra_target: Option<SegmentIndex>,
    /// Read-only workspace metadata with additional information, or `None` if nothing was present.
    /// If this is `Some()` the `kind` is always [`WorkspaceKind::Managed`]
    pub metadata: Option<ref_metadata::Workspace>,
}

/// A classifier for the workspace.
#[derive(Debug, Clone)]
pub enum WorkspaceKind {
    /// The `HEAD` is pointing to a dedicated workspace reference, like `refs/heads/gitbutler/workspace`.
    Managed {
        /// The name of the reference pointing to the workspace commit. Useful for deriving the workspace name.
        ref_name: gix::refs::FullName,
    },
    /// A segment is checked out directly.
    ///
    /// It can be inside or outside a workspace.
    /// If the respective segment is [not named](Workspace::ref_name), this means the `HEAD` id detached.
    /// The commit that the working tree is at is always implied to be the first commit of the [`StackSegment`].
    AdHoc,
}

impl WorkspaceKind {
    fn managed(ref_name: &Option<gix::refs::FullName>) -> anyhow::Result<Self> {
        Ok(WorkspaceKind::Managed {
            ref_name: ref_name
                .as_ref()
                .cloned()
                .context("BUG: managed workspaces must always be on a named segment")?,
        })
    }
}

/// Information about the target reference.
#[derive(Debug, Clone)]
pub struct Target {
    /// The name of the target branch, i.e. the branch that all [Stacks](Stack) want to get merged into.
    /// Typically, this is `origin/main`.
    pub ref_name: gix::refs::FullName,
    /// The index to the respective segment in the graph.
    pub segment_index: SegmentIndex,
    /// The amount of commits that aren't reachable by any segment in the workspace, they are in its future.
    pub commits_ahead: usize,
}

impl Target {
    /// Return `None` if `ref_name` wasn't found as segment in `graph`.
    /// This can happen if a reference is configured, but not actually present as reference.
    fn from_ref_name(ref_name: &gix::refs::FullName, graph: &Graph) -> Option<Self> {
        let target_segment = graph.inner.node_indices().find_map(|n| {
            let s = &graph[n];
            (s.ref_name.as_ref() == Some(ref_name)).then_some(s)
        })?;
        Some(Target {
            ref_name: ref_name.to_owned(),
            segment_index: target_segment.id,
            commits_ahead: {
                // Find all remote commits but stop traversing when there is segments without remotes.
                let mut count = 0;
                graph.visit_all_segments_until(target_segment.id, Direction::Outgoing, |s| {
                    let remote_commits = s.commits.iter().filter(|c| c.flags.is_remote()).count();
                    count += remote_commits;
                    remote_commits != s.commits.len()
                });
                count
            },
        })
    }
}

impl Graph {
    /// Analyse the current graph starting at its [entrypoint](Self::lookup_entrypoint()).
    ///
    /// No matter what, each location of `HEAD`, which corresponds to the entrypoint, can be represented as workspace.
    /// Further, the most expensive operations we perform to query additional commit information by reading it, but we
    /// only do so on the ones that the user can interact with.
    ///
    /// The [`extra_target`](crate::init::Options::extra_target) options extends the workspace to include that target as base.
    /// This affects what we consider to be the part of the workspace.
    /// Typically, that's a previous location of the target segment.
    #[instrument(skip(self), err(Debug))]
    pub fn to_workspace(&self) -> anyhow::Result<Workspace<'_>> {
        let (kind, metadata, mut ws_tip_segment, entrypoint_sidx, entrypoint_first_commit_flags) = {
            let ep = self.lookup_entrypoint()?;
            match ep.segment.workspace_metadata() {
                None => {
                    // Skip over empty segments.
                    if let Some((maybe_integrated_flags, sidx_of_flags)) = self
                        .first_commit_or_find_along_first_parent(ep.segment_index)
                        .map(|(c, sidx)| (c.flags, sidx))
                        .filter(|(f, _sidx)| f.contains(CommitFlags::InWorkspace))
                    {
                        // search the (for now just one) workspace upstream and use it instead,
                        // mark this segment as entrypoint.
                        // Note that at this time the entrypoint could still be below the fork-point of the workspace.
                        let ws_segment = self
                            .find_segment_upwards(sidx_of_flags, |s| {
                                s.workspace_metadata().is_some()
                            })
                            .with_context(|| {
                                format!(
                                    "BUG: should have found upstream workspace segment from {:?} as commit is marked as such",
                                    sidx_of_flags
                                )
                            })?;

                        (
                            WorkspaceKind::managed(&ws_segment.ref_name)?,
                            ws_segment.workspace_metadata().cloned(),
                            ws_segment,
                            Some(ep.segment_index),
                            maybe_integrated_flags,
                        )
                    } else {
                        (
                            WorkspaceKind::AdHoc,
                            None,
                            ep.segment,
                            None,
                            CommitFlags::empty(),
                        )
                    }
                }
                Some(meta) => (
                    WorkspaceKind::managed(&ep.segment.ref_name)?,
                    Some(meta.clone()),
                    ep.segment,
                    None,
                    CommitFlags::empty(),
                ),
            }
        };

        let mut ws = Workspace {
            graph: self,
            id: ws_tip_segment.id,
            kind,
            stacks: vec![],
            target: metadata
                .as_ref()
                .and_then(|md| Target::from_ref_name(md.target_ref.as_ref()?, self)),
            extra_target: self.extra_target,
            metadata,
            lower_bound_segment_id: None,
            lower_bound: None,
        };

        let ws_lower_bound = if ws.is_managed() {
            self.compute_lowest_base(ws.id, ws.target.as_ref(), self.extra_target)
                .or_else(|| {
                    // target not available? Try the base of the workspace itself
                    if self
                        .inner
                        .neighbors_directed(ws_tip_segment.id, Direction::Outgoing)
                        .count()
                        == 1
                    {
                        None
                    } else {
                        self.inner
                            .neighbors_directed(ws_tip_segment.id, Direction::Outgoing)
                            .reduce(|a, b| self.first_merge_base(a, b).unwrap_or(a))
                            .and_then(|base| self[base].commits.first().map(|c| (c.id, base)))
                    }
                })
        } else {
            None
        };

        (ws.lower_bound, ws.lower_bound_segment_id) = ws_lower_bound
            .map(|(a, b)| (Some(a), Some(b)))
            .unwrap_or_default();

        // The entrypoint is integrated and has a workspace above it.
        // Right now we would be using it, but will discard it the entrypoint is *at* or *below* the merge-base,
        // but only if it doesn't obviously belong to the workspace, by having metadata.
        if let Some(((_lowest_base, lowest_base_sidx), ep_sidx)) = ws_lower_bound
            .filter(|(_, ep_sidx)| {
                entrypoint_first_commit_flags.contains(CommitFlags::Integrated)
                    && self[*ep_sidx].metadata.is_none()
            })
            .zip(entrypoint_sidx)
        {
            if ep_sidx == lowest_base_sidx
                || self
                    .find_map_downwards_along_first_parent(ep_sidx, |s| {
                        (s.id == lowest_base_sidx).then_some(())
                    })
                    .is_none()
            {
                // We cannot reach the lowest workspace base, by definition reachable through any path downward,
                // so we are outside the workspace limits which is above us. Turn the data back into entrypoint-only.
                let Workspace {
                    graph: _,
                    id,
                    kind: head,
                    stacks: _,
                    target,
                    metadata,
                    extra_target: _,
                    lower_bound,
                    lower_bound_segment_id,
                } = &mut ws;
                *id = ep_sidx;
                *head = WorkspaceKind::AdHoc;
                *target = None;
                *metadata = None;
                ws_tip_segment = &self[ep_sidx];
                *lower_bound = None;
                *lower_bound_segment_id = None;
            }
        }

        if ws.is_managed() {
            let (_lowest_base, lowest_base_sidx) =
                ws_lower_bound.map_or((None, None), |(base, sidx)| (Some(base), Some(sidx)));
            for stack_top_sidx in self
                .inner
                .neighbors_directed(ws_tip_segment.id, Direction::Outgoing)
            {
                let stack_segment = &self[stack_top_sidx];
                let has_seen_base = RefCell::new(false);
                ws.stacks.extend(
                    self.collect_stack_segments(
                        stack_top_sidx,
                        entrypoint_sidx,
                        |s| {
                            let stop = true;
                            // The lowest base is a segment that all stacks will run into.
                            // If we meet it, we are done. Note how we ignored the integration state
                            // as pruning of fully integrated stacks happens later.
                            if Some(s.id) == lowest_base_sidx {
                                has_seen_base.replace(true);
                                return stop;
                            }
                            // Assure entrypoints get their own segments
                            if s.id != stack_top_sidx && Some(s.id) == entrypoint_sidx {
                                return stop;
                            }
                            // TODO: test for that!
                            if s.workspace_metadata().is_some() {
                                return stop;
                            }
                            match (
                                &stack_segment.ref_name,
                                s.ref_name
                                    .as_ref()
                                    .filter(|rn| rn.category() == Some(Category::LocalBranch)),
                            ) {
                                (Some(_), Some(_)) | (None, Some(_)) => stop,
                                (Some(_), None) | (None, None) => false,
                            }
                        },
                        |s| {
                            !*has_seen_base.borrow()
                                && self
                                    .inner
                                    .neighbors_directed(s.id, Direction::Incoming)
                                    .all(|n| n.id() != ws_tip_segment.id)
                        },
                        |s| Some(s.id) == ws.lower_bound_segment_id && s.metadata.is_none(),
                    )?
                    .map(|segments| Stack::from_base_and_segments(&self.inner, segments)),
                );
            }
        } else {
            let start = ws_tip_segment;
            ws.stacks.extend(
                // TODO: This probably depends on more factors, could have relationship with remote tracking branch.
                self.collect_stack_segments(
                    start.id,
                    None,
                    |s| {
                        let stop = true;
                        // TODO: test for that!
                        if s.workspace_metadata().is_some() {
                            return stop;
                        }
                        match (&start.ref_name, &s.ref_name) {
                            (Some(_), Some(_)) | (None, Some(_)) => stop,
                            (Some(_), None) | (None, None) => false,
                        }
                    },
                    // We keep going until depletion
                    |_s| true,
                    // Never discard stacks
                    |_s| false,
                )?
                .map(|segments| Stack::from_base_and_segments(&self.inner, segments)),
            );
        }

        ws.mark_remote_reachability()?;
        Ok(ws)
    }

    /// Compute the lowest base (i.e. the highest generation) between the `ws_tip` of a top-most segment of the workspace,
    /// another `target` segment, and any amount of `additional` segments which could be *past targets* to keep
    /// an artificial lower base for consistency.
    ///
    /// Returns `Some((lowest_base, segment_idx_with_lowest_base))`.
    ///
    /// ## Note
    ///
    /// This is a **merge-base octopus** effectively, and works without generation numbers.
    // TODO: actually compute the lowest base, see `first_merge_base()` which should be `lowest_merge_base()` by itself,
    //       accounting for finding the lowest of all merge-bases which would be assumed to be reachable by all segments
    //       searching downward, a necessary trait for many search problems.
    fn compute_lowest_base(
        &self,
        ws_tip: SegmentIndex,
        target: Option<&Target>,
        additional: impl IntoIterator<Item = SegmentIndex>,
    ) -> Option<(gix::ObjectId, SegmentIndex)> {
        // It's important to not start from the tip, but instead find paths to the merge-base from each stack individually.
        // Otherwise, we may end up with a short path to a segment that isn't actually reachable by all stacks.
        let stacks = self.inner.neighbors_directed(ws_tip, Direction::Outgoing);
        let mut count = 0;
        let base = stacks
            .chain(target.map(|t| t.segment_index))
            .chain(additional)
            .inspect(|_| count += 1)
            .reduce(|a, b| self.first_merge_base(a, b).unwrap_or(a))?;

        if count < 2 || base == ws_tip {
            None
        } else {
            self.first_commit_or_find_along_first_parent(base)
                .map(|(c, sidx)| (c.id, sidx))
        }
    }

    /// Compute the loweset merge-base between two segments.
    /// Such a merge-base is reachable from all possible paths from `a` and `b`.
    ///
    /// We know this works as all branching and merging is represented by a segment.
    /// Thus, the merge-base is always the first commit of the returned segment
    // TODO: should be multi, with extra segments as third parameter
    // TODO: actually find the lowest merge-base, right now it just finds the first merge-base, but that's not
    //       the lowest.
    fn first_merge_base(&self, a: SegmentIndex, b: SegmentIndex) -> Option<SegmentIndex> {
        // TODO(perf): improve this by allowing to set bitflags on the segments themselves, to allow
        //       marking them accordingly, just like Git does.
        //       Right now we 'emulate' bitflags on pre-allocated data with two data sets, expensive
        //       in comparison.
        //       And yes, let's avoid `gix::Repository::merge_base` as we have free
        //       generation numbers here and can avoid work duplication.
        let mut segments_reachable_by_b = BTreeSet::new();
        self.visit_all_segments_until(b, Direction::Outgoing, |s| {
            segments_reachable_by_b.insert(s.id);
            // Collect everything, keep it simple.
            // This is fast* as completely in memory.
            // *means slow compared to an array traversal with memory locality.
            false
        });

        let mut candidate = None;
        self.visit_all_segments_until(a, Direction::Outgoing, |s| {
            if candidate.is_some() {
                return true;
            }
            let prune = segments_reachable_by_b.contains(&s.id);
            if prune {
                candidate = Some(s.id);
            }
            prune
        });
        if candidate.is_none() {
            // TODO: improve this - workspaces shouldn't be like this but if they are, do we deal with it well?
            tracing::warn!(
                "Couldn't find merge-base between segments {a:?} and {b:?} - this might lead to unexpected results"
            )
        }
        candidate
    }
}

/// Traversals
impl Graph {
    /// Return the ancestry of `start` along the first parents, itself included, until `stop` returns `true`.
    /// Also return the segment that we stopped at.
    /// **Important**: `stop` is not called with `start`, this is a feature.
    ///
    /// Note that the traversal assumes as well-segmented graph without cycles.
    fn collect_first_parent_segments_until<'a>(
        &'a self,
        start: &'a Segment,
        mut stop: impl FnMut(&Segment) -> bool,
    ) -> (Vec<&'a Segment>, Option<&'a Segment>) {
        let mut out = vec![start];
        let mut edge = self
            .inner
            .edges_directed(start.id, Direction::Outgoing)
            .last();
        let mut stopped_at = None;
        let mut seen = BTreeSet::new();
        while let Some(first_edge) = edge {
            let next = &self[first_edge.target()];
            if stop(next) {
                stopped_at = Some(next);
                break;
            }
            out.push(next);
            if seen.insert(next.id) {
                edge = self
                    .inner
                    .edges_directed(next.id, Direction::Outgoing)
                    .last();
            }
        }
        (out, stopped_at)
    }

    /// Visit the ancestry of `start` along the first parents, itself included, until `stop` returns `true`.
    /// Also return the segment that we stopped at.
    /// **Important**: `stop` is not called with `start`, this is a feature.
    ///
    /// Note that the traversal assumes as well-segmented graph without cycles.
    fn visit_segments_along_first_parent_until(
        &self,
        start: SegmentIndex,
        mut stop: impl FnMut(&Segment) -> bool,
    ) {
        let mut edge = self.inner.edges_directed(start, Direction::Outgoing).last();
        let mut seen = BTreeSet::new();
        while let Some(first_edge) = edge {
            let next = &self[first_edge.target()];
            if stop(next) {
                break;
            }
            if seen.insert(next.id) {
                edge = self
                    .inner
                    .edges_directed(next.id, Direction::Outgoing)
                    .last();
            }
        }
    }

    /// Visit all segments from `start`, excluding, and return once `find` returns something mapped from the
    /// first suitable segment it encountered.
    fn find_map_downwards_along_first_parent<T>(
        &self,
        start: SegmentIndex,
        mut find: impl FnMut(&Segment) -> Option<T>,
    ) -> Option<T> {
        let mut out = None;
        self.visit_segments_along_first_parent_until(start, |s| {
            if let Some(res) = find(s) {
                out = Some(res);
                true
            } else {
                false
            }
        });
        out
    }

    /// Return `(commit, start)` if `start` has a commit, or find the first commit downstream along the first parent.
    pub(crate) fn first_commit_or_find_along_first_parent(
        &self,
        start: SegmentIndex,
    ) -> Option<(&crate::Commit, SegmentIndex)> {
        self[start].commits.first().map(|c| (c, start)).or_else(|| {
            self.find_map_downwards_along_first_parent(start, |s| s.commits.first().map(|_c| s.id))
                // workaround borrowchk
                .map(|sidx| (self[sidx].commits.first().expect("present"), sidx))
        })
    }

    /// Return `OK(None)` if the post-process discarded this segment after collecting it in full as it was not
    /// local a local branch.
    ///
    /// `entrypoint_sidx` is passed to set the collected segment as entrypoint automatically.
    ///
    /// `is_one_past_end_of_stack_segment(s)` returns `true` if the graph segment `s` should be considered past the
    /// currently collected stack segment. If `false` is returned, it will become part of the current stack segment.
    /// It's not called for the first segment, so you can use it to compare the first with other segments.
    ///
    /// `starts_next_stack_segment(s)` returns `true` if a new stack segment should be started with `s` as first member,
    /// or `false` if the stack segments are complete and with it all stack segments.
    ///
    /// `discard_stack(stack_segment)` returns `true` if after collecting everything, we'd still want to discard the
    /// whole stack due to custom rules, after assuring the stack segment is no entrypoint.
    /// It's also called to determine if a stack-segment (from the bottom of the stack upwards) should be discarded.
    /// If the stack is empty at the end, it will be discarded in full.
    fn collect_stack_segments(
        &self,
        from: SegmentIndex,
        entrypoint_sidx: Option<SegmentIndex>,
        mut is_one_past_end_of_stack_segment: impl FnMut(&Segment) -> bool,
        mut starts_next_stack_segment: impl FnMut(&Segment) -> bool,
        mut discard_stack: impl FnMut(&StackSegment) -> bool,
    ) -> anyhow::Result<Option<Vec<StackSegment>>> {
        // TODO: Test what happens if a workspace commit is pointed at by a different ref (which is the entrypoint).
        let mut out = Vec::new();
        let mut next = Some(from);
        while let Some(from) = next.take() {
            let start = &self[from];
            let (segments, stopped_at) = self
                .collect_first_parent_segments_until(start, &mut is_one_past_end_of_stack_segment);
            let mut segment = StackSegment::from_graph_segments(&segments, self)?;
            if entrypoint_sidx.is_some_and(|id| segment.id == id) {
                segment.is_entrypoint = true;
            }
            out.push(segment);
            next = stopped_at
                .filter(|s| starts_next_stack_segment(s))
                .map(|s| s.id);
        }

        fn is_entrypoint_or_local(s: &StackSegment) -> bool {
            if s.is_entrypoint {
                return true;
            }
            s.ref_name
                .as_ref()
                .and_then(|rn| rn.category())
                .is_none_or(|c| c == Category::LocalBranch)
        }

        // Prune empty invalid ones from the front as cleanup.
        // This isn't an issue for algorithms as they always see the full version.
        // TODO: remove this once we don't have remotes in a workspace because traversal logic can do it better.
        if let Some(end) = out
            .iter()
            .enumerate()
            .take_while(|(_idx, s)| s.commits.is_empty() && !is_entrypoint_or_local(s))
            .map(|(idx, _s)| idx + 1)
            .last()
        {
            out.drain(..end);
        }

        // Definitely remove non-local empties from behind.
        // TODO: revise this
        if let Some(new_len) = out
            .iter()
            .enumerate()
            .rev()
            .take_while(|(_idx, s)| s.commits.is_empty() && !is_entrypoint_or_local(s))
            .last()
            .map(|(idx, _s)| idx)
        {
            out.truncate(new_len);
        }

        // TODO: remove the hack of avoiding empty segments as special case, remove .is_empty() condition
        let is_pruned = |s: &StackSegment| !s.commits.is_empty() && !is_entrypoint_or_local(s);
        // Prune the whole stack if we start with unwanted segments.
        if out
            .first()
            .is_some_and(|s| is_pruned(s) || discard_stack(s))
        {
            tracing::warn!(
                "Ignoring stack {:?} ({:?}) as it is pruned",
                out.first().and_then(|s| s.ref_name.as_ref()),
                from,
            );
            return Ok(None);
        }

        // We may have picked up unwanted segments, if the graph isn't perfectly clean
        // TODO: remove this to rather assure that non-local branches aren't linked up that way.
        if let Some(new_len) = out
            .iter()
            .enumerate()
            .rev()
            .take_while(|(_idx, s)| is_pruned(s))
            .last()
            .map(|(idx, _s)| idx)
        {
            out.truncate(new_len);
        }
        Ok((!out.is_empty()).then_some(out))
    }

    /// Visit all segments across all connections, including `start` and return the segment for which `f(segment)` returns `true`.
    /// There is no traversal pruning.
    pub(crate) fn find_segment_upwards(
        &self,
        start: SegmentIndex,
        mut f: impl FnMut(&Segment) -> bool,
    ) -> Option<&Segment> {
        let mut next = VecDeque::new();
        next.push_back(start);
        let mut seen = BTreeSet::new();
        while let Some(next_sidx) = next.pop_front() {
            let s = &self[next_sidx];
            if f(s) {
                return Some(s);
            }
            next.extend(
                self.inner
                    .neighbors_directed(next_sidx, Direction::Incoming)
                    .filter(|n| seen.insert(*n)),
            );
        }
        None
    }
}

/// More processing
impl Workspace<'_> {
    // NOTE: it's a disadvantage to not do this on graph level - then all we'd need is
    //       - a sibling_sidx to know which segment belongs to our remote tracking ref (for ease of use)
    //       - an identity set for each remote ref
    //       - a field that tells us the identity bit on the remote segment, so we can check if it's set.
    //       Now we basically re-do the remote tracking in the workspace projection, which is always a bit
    //       awkward to do.
    //      And… that's why we do it on graph level, but map back to the workspace using segment ids.
    fn mark_remote_reachability(&mut self) -> anyhow::Result<()> {
        let remote_refs: Vec<_> = self
            .stacks
            .iter()
            .flat_map(|s| {
                s.segments.iter().filter_map(|s| {
                    s.remote_tracking_ref_name
                        .as_ref()
                        .cloned()
                        .zip(s.sibling_segment_id)
                })
            })
            .collect();
        let graph = self.graph;
        for (remote_tracking_ref_name, remote_sidx) in remote_refs {
            let mut remote_commits = Vec::new();
            let mut may_take_commits_from_first_remote = graph[remote_sidx].commits.is_empty();
            graph.visit_all_segments_until(remote_sidx, Direction::Outgoing, |s| {
                let prune = !s.commits.iter().all(|c| c.flags.is_remote())
                    // Do not 'steal' commits from other known remote segments while they are officially connected,
                    // unless we started out empty. That means ambiguous ownership, as multiple remotes point
                    // to the same commit.
                    || {
                    let mut prune = s.id != remote_sidx
                    && s.ref_name
                    .as_ref()
                    .is_some_and(|orn| orn.category() == Some(Category::RemoteBranch));
                    if prune && may_take_commits_from_first_remote {
                        prune = false;
                        may_take_commits_from_first_remote = false;
                    }
                    prune
                };
                if prune {
                    // See if this segment links to a commit we know as local, and mark it accordingly,
                    // along with all segments in that stack.
                    for stack in &mut self.stacks {
                        let Some((first_segment, first_commit_index)) =
                            stack.segments.iter().enumerate().find_map(|(os_idx, os)| {
                                os.commits_by_segment
                                    .iter()
                                    .find_map(|(sidx, commit_ofs)| {
                                        (*sidx == s.id).then_some(commit_ofs)
                                    })
                                    .map(|commit_ofs| (os_idx, *commit_ofs))
                            })
                        else {
                            continue;
                        };

                        let mut first_commit_index = Some(first_commit_index);
                        for segment in &mut stack.segments[first_segment..] {
                            let remote_reachable = StackCommitFlags::ReachableByRemote
                                | if segment.remote_tracking_ref_name.as_ref()
                                    == Some(&remote_tracking_ref_name)
                                {
                                    StackCommitFlags::ReachableByMatchingRemote
                                } else {
                                    StackCommitFlags::empty()
                                };
                            for commit in &mut segment.commits
                                [first_commit_index.take().unwrap_or_default()..]
                            {
                                commit.flags |= remote_reachable;
                            }
                        }
                        // keep looking - other stacks can repeat the segment!
                        continue;
                    }
                } else {
                    for commit in &s.commits {
                        remote_commits.push(StackCommit::from_graph_commit(commit));
                    }
                }
                prune
            });

            // Have to keep looking for matching segments, they can be mentioned multiple times.
            let mut found_segment = false;
            let remote_commits: Vec<_> = remote_commits.into_iter().collect::<Result<_, _>>()?;
            for local_segment_with_this_remote in self.stacks.iter_mut().flat_map(|stack| {
                stack.segments.iter_mut().filter_map(|s| {
                    (s.remote_tracking_ref_name.as_ref() == Some(&remote_tracking_ref_name))
                        .then_some(s)
                })
            }) {
                found_segment = true;
                local_segment_with_this_remote.commits_on_remote = remote_commits.clone();
            }
            if !found_segment {
                tracing::error!(
                    "BUG: Couldn't find local segment with remote tracking ref '{rn}' - remote commits for it seem to be missing",
                    rn = remote_tracking_ref_name.as_bstr()
                );
            }
        }
        Ok(())
    }
}

/// Query
impl Workspace<'_> {
    /// Return `true` if this workspace is managed, meaning we control certain aspects of it.
    /// If `false`, we are more conservative and may not support all features.
    pub fn is_managed(&self) -> bool {
        matches!(self.kind, WorkspaceKind::Managed { .. })
    }

    /// Return the name of the workspace reference by looking our segment up in `graph`.
    /// Note that for managed workspaces, this can be retrieved via [`WorkspaceKind::Managed`].
    /// Note that it can be expected to be set on any workspace, but the data would allow it to not be set.
    pub fn ref_name<'a>(&self, graph: &'a Graph) -> Option<&'a gix::refs::FullNameRef> {
        graph[self.id].ref_name.as_ref().map(|rn| rn.as_ref())
    }
}

/// Debugging
impl Workspace<'_> {
    /// Produce a distinct and compressed debug string to show at a glance what the workspace is about.
    pub fn debug_string(&self) -> String {
        let graph = self.graph;
        let (name, sign) = match &self.kind {
            WorkspaceKind::Managed { ref_name } => (Graph::ref_debug_string(ref_name), "🏘️"),
            WorkspaceKind::AdHoc => (
                graph[self.id]
                    .ref_name
                    .as_ref()
                    .map_or("DETACHED".into(), Graph::ref_debug_string),
                "⌂",
            ),
        };
        let target = self.target.as_ref().map_or_else(
            || "!".to_string(),
            |t| {
                format!(
                    "{target}{ahead}",
                    target = t.ref_name,
                    ahead = if t.commits_ahead == 0 {
                        "".to_string()
                    } else {
                        format!("⇣{}", t.commits_ahead)
                    }
                )
            },
        );
        format!(
            "{meta}{sign}:{id}:{name} <> ✓{target}{bound}",
            meta = if self.metadata.is_some() { "📕" } else { "" },
            id = self.id.index(),
            bound = self
                .lower_bound
                .map(|base| format!(" on {}", base.to_hex_with_len(7)))
                .unwrap_or_default()
        )
    }
}

impl std::fmt::Debug for Workspace<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(&format!("Workspace({})", self.debug_string()))
            .field("id", &self.id.index())
            .field("stacks", &self.stacks)
            .field("metadata", &self.metadata)
            .finish()
    }
}
