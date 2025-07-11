use crate::init::types::{EdgeOwned, TopoWalk};
use crate::init::walk::disambiguate_refs_by_branch_metadata;
use crate::init::{PetGraph, branch_segment_from_name_and_meta, remotes};
use crate::{Commit, CommitFlags, CommitIndex, Edge, Graph, SegmentIndex, SegmentMetadata};
use but_core::{RefMetadata, ref_metadata};
use gix::prelude::ObjectIdExt;
use gix::reference::Category;
use petgraph::Direction;
use petgraph::prelude::EdgeRef;
use std::collections::{BTreeMap, BTreeSet};
use tracing::instrument;

/// Processing
impl Graph {
    /// Now that the graph is complete, perform additional structural improvements with
    /// the requirement of them to be computationally cheap.
    #[instrument(skip(self, meta, repo), err(Debug))]
    pub(super) fn post_processed(
        mut self,
        meta: &impl RefMetadata,
        tip: gix::ObjectId,
        repo: &gix::Repository,
        symbolic_remote_names: &[String],
        configured_remote_tracking_branches: &BTreeSet<gix::refs::FullName>,
        inserted_proxy_segments: Vec<SegmentIndex>,
    ) -> anyhow::Result<Self> {
        // For the first id to be inserted into our entrypoint segment, set index.
        if let Some((segment, ep_commit)) = self.entrypoint.as_mut() {
            *ep_commit = self
                .inner
                .node_weight(*segment)
                .and_then(|s| s.commit_index_of(tip));
        }

        // All non-workspace fixups must come first, otherwise the workspace handling might
        // differ as it relies on non-anonymous segments much more.
        self.fixup_segment_names(meta, inserted_proxy_segments);
        // We perform view-related updates here for convenience, but in theory
        // they should have nothing to do with a standard graph. Move it out if needed.
        self.workspace_upgrades(meta, repo)?;

        // However, when it comes to using remotes to disambiguate, it's better to
        // *not* do that before workspaces are sorted as it might incorrectly place
        // a segment on top of another one, setting a first-second relationship that isn't
        // what we have in the workspace metadata, which then also can't set it anymore
        // because it can't reorder existing empty segments (which are not natural).
        self.improve_remote_segments(
            repo,
            symbolic_remote_names,
            configured_remote_tracking_branches,
        )?;

        Ok(self)
    }

    /// To keep it simple, the iteration will not always create perfect segment names right away so we
    /// fix it in post.
    ///
    /// * segments are anonymous even though there is an unambiguous name for its first parent.
    ///   These segments sometimes are inserted to assure workspace segments don't own non-workspace commits.
    /// * segments have a name, but the same name is still visible in the refs of the first commit.
    ///
    /// Only perform disambiguation on proxy segments (i.e. those inserted segments to prevent commit-ownership).
    fn fixup_segment_names(
        &mut self,
        meta: &impl RefMetadata,
        inserted_proxy_segments: Vec<SegmentIndex>,
    ) {
        // TODO(borrowchk): perform a walk instead of collect vec.
        for sidx in self.inner.node_indices().collect::<Vec<_>>() {
            let s = &mut self.inner[sidx];
            let Some(first_commit) = s.commits.first_mut() else {
                continue;
            };
            if let Some(srn) = &s.ref_name {
                if let Some(pos) = first_commit.refs.iter().position(|rn| rn == srn) {
                    first_commit.refs.remove(pos);
                }
            } else {
                match first_commit.refs.len() {
                    0 => continue,
                    1 => {
                        if first_commit
                            .refs
                            .first()
                            .is_some_and(|rn| rn.category() == Some(Category::LocalBranch))
                        {
                            s.ref_name = first_commit.refs.pop()
                        }
                    }
                    _ => {
                        if !inserted_proxy_segments.contains(&sidx) {
                            continue;
                        }
                        let Some((rn, metadata)) =
                            disambiguate_refs_by_branch_metadata(first_commit.refs.iter(), meta)
                        else {
                            continue;
                        };

                        s.metadata = metadata;
                        first_commit.refs.retain(|crn| crn != &rn);
                        s.ref_name = Some(rn);
                    }
                }
            }
        }
    }

    /// Find all *unique* commits (along with their owning segments) where we can look for references that are to be
    /// spread out as independent branches.
    fn candidates_for_independent_branches_in_workspace(
        &self,
        ws_sidx: SegmentIndex,
        target: Option<&crate::projection::Target>,
        ws_stacks: &[crate::projection::Stack],
        repo: &gix::Repository,
    ) -> anyhow::Result<Vec<SegmentIndex>> {
        let mut out: Vec<_> = ws_stacks
            .iter()
            .filter_map(|s| {
                s.base_segment_id.and_then(|sidx| {
                    let base_segment = &self[sidx];
                    base_segment
                        .commit_index_of(s.base.expect("must be set if sidx is set"))
                        .map(|cidx| (sidx, &base_segment.commits[cidx]))
                })
            })
            .collect();
        out.sort_by_key(|t| t.1.id);
        out.dedup_by_key(|t| t.1.id);

        let convert = |out: Vec<(SegmentIndex, &Commit)>| out.into_iter().map(|t| t.0).collect();

        if !out.is_empty() {
            return Ok(convert(out));
        }

        match target {
            None => {
                let Some(commit_with_refs) =
                    self[ws_sidx].commits.first().filter(|c| !c.refs.is_empty())
                else {
                    return Ok(convert(out));
                };

                // Never create anything on top of managed commits.
                if crate::projection::commit::is_managed_workspace_by_message(
                    commit_with_refs
                        .id
                        .attach(repo)
                        .object()?
                        .try_into_commit()?
                        .message_raw()?,
                ) {
                    return Ok(convert(out));
                }

                // This means we are managed, but have lost our workspace commit, instead we
                // own a commit. This really shouldn't happen.
                tracing::warn!(
                    "Workspace segment {ws_sidx:?} is owning a non-workspace commit\
                     - this shouldn't be possible"
                )
            }
            Some(target) => {
                let target_rtb = &self[target.segment_index];
                out.extend(
                    self.first_commit_or_find_along_first_parent(target_rtb.id)
                        .map(|(c, sidx)| (sidx, c)),
                );
            }
        }
        Ok(convert(out))
    }

    /// Perform operations on the current workspace, or do nothing if there is None that we would consider one.
    ///
    /// * insert empty segments as defined by the workspace that affects its downstream.
    /// * put workspace connection into the order defined in the workspace metadata.
    fn workspace_upgrades(
        &mut self,
        meta: &impl RefMetadata,
        repo: &gix::Repository,
    ) -> anyhow::Result<()> {
        let Some((ws_sidx, ws_stacks, ws_data, ws_target)) =
            self.to_workspace().ok().and_then(|mut ws| {
                let md = ws.metadata.take();
                md.map(|d| (ws.id, ws.stacks, d, ws.target))
            })
        else {
            return Ok(());
        };

        fn find_all_desired_stack_refs_in_commit(
            ws_data: &ref_metadata::Workspace,
            commit_refs: &[gix::refs::FullName],
        ) -> impl Iterator<Item = Vec<gix::refs::FullName>> {
            ws_data.stacks.iter().filter_map(|stack| {
                let matching_refs: Vec<_> = stack
                    .branches
                    .iter()
                    .filter_map(|s| commit_refs.iter().find(|rn| *rn == &s.ref_name).cloned())
                    .collect();
                (!matching_refs.is_empty()).then_some(matching_refs)
            })
        }
        // Setup independent stacks, first by looking at potential bases.
        for base_sidx in self.candidates_for_independent_branches_in_workspace(
            ws_sidx,
            ws_target.as_ref(),
            &ws_stacks,
            repo,
        )? {
            let matching_refs_per_stack: Vec<_> =
                find_all_desired_stack_refs_in_commit(&ws_data, &self[base_sidx].commits[0].refs)
                    .collect();
            for refs_for_independent_branches in matching_refs_per_stack {
                let new_refs = create_independent_segments(
                    self,
                    ws_sidx,
                    base_sidx,
                    refs_for_independent_branches,
                    meta,
                )?;
                self[base_sidx].commits[0].refs = new_refs;
            }
        }

        // Setup dependent stacks based on searching refs on existing workspace commits.
        // Note that we can still source names from previously used stacks just to be able to capture more
        // of the original intent, despite the graph having changed. This works because in the end, we are consuming
        // refs on commits that can't be re-used once they have been moved into their own segment.
        for ws_segment_sidx in ws_stacks.iter().flat_map(|stack| {
            stack
                .segments
                .iter()
                .flat_map(|segment| segment.commits_by_segment.iter().map(|t| t.0))
        }) {
            // Find all commit-refs which are mentioned in ws_data.stacks, for simplicity in any stack that matches (for now).
            // Stacks shouldn't be so different that they don't match reality anymore and each mutation has to re-set them to
            // match reality.
            let mut current_above = ws_segment_sidx;
            let mut truncate_commits_from = None;
            for commit_idx in 0..self[ws_segment_sidx].commits.len() {
                let commit = &self[ws_segment_sidx].commits[commit_idx];
                let has_inserted_segment_above = current_above != ws_segment_sidx;
                let Some(refs_for_dependent_branches) =
                    find_all_desired_stack_refs_in_commit(&ws_data, &commit.refs).next()
                else {
                    // Now we have to assign this uninteresting commit to the last created segment, if there was one.
                    if has_inserted_segment_above {
                        self.push_commit_and_reconnect_outgoing(
                            commit.clone(),
                            current_above,
                            (ws_segment_sidx, commit_idx),
                        );
                    }
                    continue;
                };

                // In ws-stack segment order, map all the indices from top to bottom
                let new_above = maybe_create_multiple_segments(
                    self,
                    current_above,
                    ws_segment_sidx,
                    commit_idx,
                    refs_for_dependent_branches,
                    meta,
                )?;
                // Did it actually do something?
                if new_above == Some(current_above) {
                    if has_inserted_segment_above {
                        self.push_commit_and_reconnect_outgoing(
                            self[ws_segment_sidx].commits[commit_idx].clone(),
                            current_above,
                            (ws_segment_sidx, commit_idx),
                        );
                    }
                    continue;
                }
                current_above = new_above.unwrap_or(current_above);
                // TODO(test): try with two commits, 1 (a,b,c) and 2 (c,d,e) to validate the commit-stealing works.
                truncate_commits_from.get_or_insert(commit_idx);
            }
            if let Some(truncate_from) = truncate_commits_from {
                let segment = &mut self[ws_segment_sidx];
                // Keep only the commits that weren't reassigned to other segments.
                segment.commits.truncate(truncate_from);
                delete_anon_if_empty_and_reconnect(self, ws_segment_sidx);
            }
        }

        // Redo workspace outgoing connections according to desired stack order.
        let mut edges_pointing_to_named_segment = self
            .edges_directed_in_order_of_creation(ws_sidx, Direction::Outgoing)
            .into_iter()
            .map(|e| {
                let rn = self[e.target()].ref_name.clone();
                (e.id(), e.target(), rn)
            })
            .collect::<Vec<_>>();

        let edges_original_order: Vec<_> = edges_pointing_to_named_segment
            .iter()
            .map(|(_e, sidx, _rn)| *sidx)
            .collect();
        edges_pointing_to_named_segment.sort_by_key(|(_e, sidx, rn)| {
            let res = ws_data.stacks.iter().position(|s| {
                s.branches
                    .first()
                    .is_some_and(|b| Some(&b.ref_name) == rn.as_ref())
            });
            // This makes it so that edges that weren't mentioned in workspace metadata
            // retain their relative order, with first-come-first-serve semantics.
            // The expected case is that each segment is defined.
            res.or_else(|| {
                edges_original_order
                    .iter()
                    .position(|sidx_for_order| sidx_for_order == sidx)
            })
        });

        for (eid, target_sidx, _) in edges_pointing_to_named_segment {
            let weight = self
                .inner
                .remove_edge(eid)
                .expect("we found the edge before");
            // Reconnect according to the new order.
            self.inner.add_edge(ws_sidx, target_sidx, weight);
        }
        Ok(())
    }

    /// Name ambiguous segments if they are reachable by remote tracking branch and
    /// if the first commit has (unambiguously) the matching local tracking branch.
    /// Also, link up all remote segments with their local ones, and vice versa.
    fn improve_remote_segments(
        &mut self,
        repo: &gix::Repository,
        symbolic_remote_names: &[String],
        configured_remote_tracking_branches: &BTreeSet<gix::refs::FullName>,
    ) -> anyhow::Result<()> {
        // Map (segment-to-be-named, [candidate-remote]), so we don't set a name if there is more
        // than one remote.
        let mut remotes_by_segment_map = BTreeMap::<
            SegmentIndex,
            Vec<(gix::refs::FullName, gix::refs::FullName, SegmentIndex)>,
        >::new();

        let mut remote_sidx_by_ref_name = BTreeMap::new();
        for (remote_sidx, remote_ref_name) in self.inner.node_indices().filter_map(|sidx| {
            self[sidx]
                .ref_name
                .as_ref()
                .filter(|rn| (rn.category() == Some(Category::RemoteBranch)))
                .map(|rn| (sidx, rn))
        }) {
            remote_sidx_by_ref_name.insert(remote_ref_name.clone(), remote_sidx);
            let start_idx = self[remote_sidx].commits.first().map(|_| 0);
            let mut walk = TopoWalk::start_from(remote_sidx, start_idx, Direction::Outgoing)
                .skip_tip_segment();

            while let Some((sidx, commit_range)) = walk.next(&self.inner) {
                let segment = &self[sidx];
                if segment.ref_name.is_some() {
                    // Assume simple linear histories - otherwise this could abort too early, and
                    // we'd need a complex traversal - not now.
                    break;
                }

                if segment.commits.is_empty() {
                    // skip over empty anonymous buckets, even though these shouldn't exist, ever.
                    tracing::warn!(
                        "Skipped segment {sidx} which was anonymous and empty",
                        sidx = sidx.index()
                    );
                    continue;
                } else if segment.commits[commit_range]
                    .iter()
                    .all(|c| c.flags.contains(CommitFlags::NotInRemote))
                {
                    // a candidate for naming, and we'd either expect all or none of the commits
                    // to be in or outside a remote.
                    let first_commit = segment.commits.first().expect("we know there is commits");
                    if let Some(local_tracking_branch) = first_commit.refs.iter().find_map(|rn| {
                        remotes::lookup_remote_tracking_branch_or_deduce_it(
                            repo,
                            rn.as_ref(),
                            symbolic_remote_names,
                            configured_remote_tracking_branches,
                        )
                        .ok()
                        .flatten()
                        .and_then(|rrn| {
                            (rrn.as_ref() == remote_ref_name.as_ref()).then_some(rn.clone())
                        })
                    }) {
                        remotes_by_segment_map.entry(sidx).or_default().push((
                            local_tracking_branch,
                            remote_ref_name.clone(),
                            remote_sidx,
                        ));
                    }
                    break;
                }
                // Assume that the segment is fully remote.
                continue;
            }
        }

        for (anon_sidx, mut disambiguated_name) in remotes_by_segment_map
            .into_iter()
            .filter(|(_, candidates)| candidates.len() == 1)
        {
            let s = &mut self[anon_sidx];
            let (local, remote, remote_sidx) =
                disambiguated_name.pop().expect("one item as checked above");
            s.ref_name = Some(local);
            s.remote_tracking_ref_name = Some(remote);
            s.sibling_segment_id = Some(remote_sidx);
            let rn = s.ref_name.as_ref().expect("just set it");
            s.commits.first_mut().unwrap().refs.retain(|crn| crn != rn);
        }

        // NOTE: setting this directly at iteration time isn't great as the post-processing then
        //       also has to deal with these implicit connections. So it's best to redo them in the end.
        let mut links_from_remote_to_local = Vec::new();
        for segment in self.inner.node_weights_mut() {
            if segment.remote_tracking_ref_name.is_some() {
                continue;
            };
            let Some(ref_name) = segment.ref_name.as_ref() else {
                continue;
            };
            segment.remote_tracking_ref_name = remotes::lookup_remote_tracking_branch_or_deduce_it(
                repo,
                ref_name.as_ref(),
                symbolic_remote_names,
                configured_remote_tracking_branches,
            )?;

            if let Some(remote_sidx) = segment
                .remote_tracking_ref_name
                .as_ref()
                .and_then(|rn| remote_sidx_by_ref_name.remove(rn))
            {
                segment.sibling_segment_id = Some(remote_sidx);
                links_from_remote_to_local.push((remote_sidx, segment.id));
            }
        }
        for (remote_sidx, local_sidx) in links_from_remote_to_local {
            self[remote_sidx].sibling_segment_id = Some(local_sidx);
        }
        Ok(())
    }

    fn push_commit_and_reconnect_outgoing(
        &mut self,
        commit: Commit,
        current_above: SegmentIndex,
        (ws_segment_sidx, commit_idx): (SegmentIndex, CommitIndex),
    ) {
        let commit_id = commit.id;
        self[current_above].commits.push(commit);
        reconnect_outgoing(
            &mut self.inner,
            (ws_segment_sidx, commit_idx),
            (current_above, commit_id),
        );
    }
}

fn delete_anon_if_empty_and_reconnect(graph: &mut Graph, sidx: SegmentIndex) {
    let segment = &graph[sidx];
    let may_delete = segment.commits.is_empty() && segment.ref_name.is_none();
    if !may_delete {
        return;
    }

    let mut outgoing = graph.inner.edges_directed(sidx, Direction::Outgoing);
    let Some(first_outgoing) = outgoing.next() else {
        return;
    };

    if outgoing.next().is_some() {
        return;
    }
    // Reconnect
    let new_target = first_outgoing.target();
    let incoming: Vec<_> = graph
        .inner
        .edges_directed(sidx, Direction::Incoming)
        .map(EdgeOwned::from)
        .collect();
    for edge in incoming.iter().rev() {
        graph.inner.add_edge(edge.source, new_target, edge.weight);
    }
    graph.inner.remove_node(sidx);

    if let Some(ep_sidx) = graph
        .entrypoint
        .as_mut()
        .map(|t| &mut t.0)
        .filter(|ep_sidx| **ep_sidx == sidx)
    {
        *ep_sidx = new_target;
    }
}

/// Create as many new segments as refs in `matching_refs`, connect them to each other in order, and finally connect them
/// with `above_idx` and `below_idx` to integrate them into the workspace that is bounded by these segments.
///
/// Returns the refs to replace the first commit in `below_sidx`, with `matching_refs` removed.
fn create_independent_segments(
    graph: &mut Graph,
    above_idx: SegmentIndex,
    below_idx: SegmentIndex,
    matching_refs: Vec<gix::refs::FullName>,
    meta: &impl RefMetadata,
) -> anyhow::Result<Vec<gix::refs::FullName>> {
    assert!(!matching_refs.is_empty());

    let mut above = above_idx;
    let mut new_refs = graph[below_idx].commits[0].refs.clone();
    for ref_name in matching_refs {
        new_refs.remove(
            new_refs
                .iter()
                .position(|rn| rn == &ref_name)
                .expect("each ref_name must be based on refs in parent commit"),
        );
        let new_segment = branch_segment_from_name_and_meta(Some((ref_name, None)), meta, None)?;
        let new_segment_sidx = graph.connect_new_segment(
            above,
            graph[above].last_commit_index(),
            new_segment,
            None,
            None,
        );
        above = new_segment_sidx;
    }
    graph.connect_segments(above, None, below_idx, Some(0));
    Ok(new_refs)
}

/// Maybe create a new stack from `N` (where `N` > 1) refs that match a ref in `ws_stack` (in the order given there), with `N-1` segments being empty on top
/// of the last one `N`.
/// `commit_parent` is the segment to use `commit_idx` on to get its data. We also use this information to re-link
/// Return `Some(bottom_segment_index)`, or `None` no ref matched commit. There may be any amount of new segments above
/// the `bottom_segment_index`.
/// Note that the Segment at `bottom_segment_index` will own `commit`.
/// Also note that we reconnect commit-by-commit, so the outer processing has to do that.
/// Note that it may avoid creating a new segment.
fn maybe_create_multiple_segments(
    graph: &mut Graph,
    mut above_idx: SegmentIndex,
    commit_parent: SegmentIndex,
    commit_idx: CommitIndex,
    mut matching_refs: Vec<gix::refs::FullName>,
    meta: &impl RefMetadata,
) -> anyhow::Result<Option<SegmentIndex>> {
    assert!(!matching_refs.is_empty());
    let commit = &graph[commit_parent].commits[commit_idx];

    let iter_len = matching_refs.len();
    // Shortcut: instead of replacing single anonymous segments, set their name.
    if iter_len == 1 && graph[above_idx].ref_name.is_none() {
        let s = &mut graph[above_idx];
        let rn = matching_refs.pop().expect("exactly viable name");
        s.metadata = meta
            .branch_opt(rn.as_ref())?
            .map(|md| SegmentMetadata::Branch(md.clone()));
        s.commits
            .first_mut()
            .expect("at least one commit")
            .refs
            .retain(|crn| crn != &rn);
        s.ref_name = rn.into();
        return Ok(Some(above_idx));
    }

    let commit = {
        let mut c = commit.clone();
        c.refs.retain(|rn| !matching_refs.contains(rn));
        c
    };
    let matching_refs = matching_refs
        .into_iter()
        .enumerate()
        .map(|(idx, ref_name)| {
            let (mut first, mut last) = (false, false);
            if idx == 0 {
                first = true;
            }
            if idx + 1 == iter_len {
                last = true;
            }
            (first, last, ref_name)
        });
    for (is_first, is_last, ref_name) in matching_refs {
        let new_segment = branch_segment_from_name_and_meta(Some((ref_name, None)), meta, None)?;
        let above_commit_idx = {
            let s = &graph[above_idx];
            let cidx = s.commit_index_of(commit.id);
            if cidx.is_some() {
                // We will take the current commit, so must commit to the one above.
                // This works just once, for the actually passed parent commit.
                cidx.and_then(|cidx| cidx.checked_sub(1))
            } else {
                // Otherwise, assure the connection is valid by using the last commit.
                s.last_commit_index()
            }
        };
        let new_segment = graph.connect_new_segment(
            above_idx,
            above_commit_idx,
            new_segment,
            is_last.then_some(0),
            is_last.then_some(commit.id),
        );
        above_idx = new_segment;
        if is_first {
            // connect incoming edges (and disconnect from source)
            // Connect to the commit if we have one.
            let edges = collect_edges_at_commit_reverse_order(
                &graph.inner,
                (commit_parent, commit_idx),
                Direction::Incoming,
            );
            for edge in &edges {
                graph.inner.remove_edge(edge.id);
            }
            for edge in edges.into_iter().rev() {
                let (target, target_cidx) = if commit_idx == 0 {
                    // the current target of the edge will be empty after we steal its commit.
                    // Thus, we want to keep pointing to it to naturally reach the commit later.
                    (edge.target, None)
                } else {
                    // The new segment is the shortest way to the commit we loose.
                    (new_segment, is_last.then_some(0))
                };
                graph.inner.add_edge(
                    edge.source,
                    target,
                    Edge {
                        src: edge.weight.src,
                        src_id: edge.weight.src_id,
                        dst: target_cidx,
                        dst_id: target_cidx.map(|_| commit.id),
                    },
                );
            }
        }
        if is_last {
            // connect outgoing edges (and disconnect them)
            let commit_id = commit.id;
            graph[new_segment].commits.push(commit);

            reconnect_outgoing(
                &mut graph.inner,
                (commit_parent, commit_idx),
                (new_segment, commit_id),
            );
            break;
        }
    }
    Ok(Some(above_idx))
}

/// This removes outgoing connections from `source_sidx` and places them on the first commit
/// of `target_sidx`.
fn reconnect_outgoing(
    graph: &mut PetGraph,
    (source_sidx, source_cidx): (SegmentIndex, CommitIndex),
    (target_sidx, target_first_commit_id): (SegmentIndex, gix::ObjectId),
) {
    let edges = collect_edges_at_commit_reverse_order(
        graph,
        (source_sidx, source_cidx),
        Direction::Outgoing,
    );
    for edge in &edges {
        graph.remove_edge(edge.id);
    }
    for edge in edges.into_iter().rev() {
        let src = graph[target_sidx].commit_index_of(target_first_commit_id);
        graph.add_edge(
            target_sidx,
            edge.target,
            Edge {
                src,
                src_id: Some(target_first_commit_id),
                dst: edge.weight.dst,
                dst_id: edge.weight.dst_id,
            },
        );
    }
}

fn collect_edges_at_commit_reverse_order(
    graph: &PetGraph,
    (segment, commit): (SegmentIndex, CommitIndex),
    direction: Direction,
) -> Vec<EdgeOwned> {
    graph
        .edges_directed(segment, direction)
        .filter(|&e| match direction {
            Direction::Incoming => e.weight().dst == Some(commit),
            Direction::Outgoing => e.weight().src == Some(commit),
        })
        .map(Into::into)
        .collect()
}
