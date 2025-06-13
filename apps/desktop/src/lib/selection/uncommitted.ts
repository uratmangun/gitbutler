import {
	hunkAssignmentAdapter,
	treeChangeAdapter,
	hunkSelectionAdapter as hunkSelectionAdapter,
	type HunkSelection,
	compositeKey,
	partialKey,
	prefixKey
} from '$lib/selection/entityAdapters';
import {
	createSelectByIds,
	createSelectByPrefix,
	createSelectNotIn
} from '$lib/state/customSelectors';
import { isDefined } from '@gitbutler/ui/utils/typeguards';
import {
	createSelector,
	createSlice,
	type EntityState,
	type PayloadAction
} from '@reduxjs/toolkit';
import type { TreeChange } from '$lib/hunks/change';
import type { HunkAssignment, HunkHeader } from '$lib/hunks/hunk';
import type { LineId } from '@gitbutler/ui/utils/diffParsing';

type UncommittedState = {
	treeChanges: EntityState<TreeChange, string>;
	hunkAssignments: EntityState<HunkAssignment, string>;
	hunkSelection: EntityState<HunkSelection, string>;
};

/**
 * State representing uncommitted changes.
 *
 * In this slice we manage a few related concepts, 1) tree changes, 2) hunk
 * assignments, and 3) hunk selections, with the intended outcome that it
 * should be easy to manage checkboxes.
 *
 * A hunk selection will always have an associated hunk assignment.
 */
export const uncommittedSlice = createSlice({
	name: 'uncommitted',
	initialState: {
		treeChanges: treeChangeAdapter.getInitialState(),
		hunkAssignments: hunkAssignmentAdapter.getInitialState(),
		hunkSelection: hunkSelectionAdapter.getInitialState()
	} as UncommittedState,
	reducers: {
		clearHunkSelection(state, action: PayloadAction<{ stackId: string | null }>) {
			state.hunkSelection = hunkSelectionAdapter.removeMany(
				state.hunkSelection,
				state.hunkSelection.ids.filter((id) => id.startsWith(`${action.payload.stackId}`))
			);
		},
		// We want to go over all the existing hunk assignments and
		// - Remove any that don't have a cooresponding id in the new assignments.
		// - Update the selections in those that have a cooresponding id in the new assignments.
		// - Add any new assignments
		update(state, action: PayloadAction<{ assignments: HunkAssignment[]; changes: TreeChange[] }>) {
			return updateAssignments(state, action);
		},
		checkLine(
			state,
			action: PayloadAction<{
				stackId: string | null;
				path: string;
				hunkHeader: HunkHeader;
				line: LineId;
			}>
		) {
			const key = compositeKey(action.payload);
			const selection = uncommittedSelectors.hunkSelection.selectById(state.hunkSelection, key);
			const { stackId, line } = action.payload;
			if (selection) {
				state.hunkSelection = hunkSelectionAdapter.upsertOne(state.hunkSelection, {
					...selection,
					lines: [...selection.lines, line]
				});
			} else {
				const assignment = uncommittedSelectors.hunkAssignments.selectById(
					state.hunkAssignments,
					key
				);
				if (!assignment) {
					throw new Error(`Expected to find assignment: ${key} `);
				}
				state.hunkSelection = hunkSelectionAdapter.addOne(state.hunkSelection, {
					stableId: assignment.id,
					stackId: stackId,
					path: assignment.path,
					assignmentId: key,
					lines: [line]
				});
			}
		},
		uncheckLine(
			state,
			action: PayloadAction<{
				stackId: string | null;
				path: string;
				hunkHeader: HunkHeader;
				line: LineId;
				allLinesInHunk: LineId[];
			}>
		) {
			const { stackId, path, hunkHeader, line, allLinesInHunk } = action.payload;
			const key = compositeKey({ stackId, path, hunkHeader });
			const selection = uncommittedSelectors.hunkSelection.selectById(state.hunkSelection, key);
			if (selection) {
				const assignment = uncommittedSelectors.hunkAssignments.selectById(
					state.hunkAssignments,
					selection.assignmentId
				);
				if (!assignment) {
					throw new Error(`Expected to find assignment: ${key} `);
				}
				if (assignment.hunkHeader === null) {
					// TODO: Validate that this never happens?
					throw new Error('Not implemented');
				}

				if (selection.lines.length === 0) {
					// No lines selected means the whole hunk is selected.
					// Unselecting one line means that all lines except that one are selected.
					const newLines = allLinesInHunk.filter(
						(l) => l.newLine !== line.newLine || l.oldLine !== line.oldLine
					);

					if (newLines.length > 0) {
						// If there are still lines selected, we update the selection.
						state.hunkSelection = hunkSelectionAdapter.upsertOne(state.hunkSelection, {
							...selection,
							lines: newLines
						});
						return;
					}

					// If there are no lines left selected, we remove the selection.
					state.hunkSelection = hunkSelectionAdapter.removeOne(
						state.hunkSelection,
						selection.assignmentId
					);
					return;
				}

				// Some lines are selected, so we remove the line from the selection.
				const newLines = selection.lines.filter(
					(l) => l.newLine !== line.newLine || l.oldLine !== line.oldLine
				);

				if (newLines.length > 0) {
					// As long as there are still lines selected, we update the selection.
					state.hunkSelection = hunkSelectionAdapter.upsertOne(state.hunkSelection, {
						...selection,
						lines: newLines
					});
					return;
				}

				// Otherwise, if there are no lines left selected, we remove the hunk completely.
				state.hunkSelection = hunkSelectionAdapter.removeOne(
					state.hunkSelection,
					selection.assignmentId
				);
			}
		},
		checkHunk(
			state,
			action: PayloadAction<{ stackId: string | null; path: string; hunkHeader: HunkHeader | null }>
		) {
			const key = compositeKey(action.payload);
			const assignment = uncommittedSelectors.hunkAssignments.selectById(
				state.hunkAssignments,
				key
			);
			if (assignment) {
				state.hunkSelection = hunkSelectionAdapter.upsertOne(state.hunkSelection, {
					stableId: assignment.id,
					stackId: action.payload.stackId,
					path: assignment.path,
					assignmentId: key,
					lines: []
				});
			}
		},
		uncheckHunk(
			state,
			action: PayloadAction<{ stackId: string | null; path: string; hunkHeader: HunkHeader | null }>
		) {
			const key = compositeKey(action.payload);
			state.hunkSelection = hunkSelectionAdapter.removeOne(state.hunkSelection, key);
		},
		checkFile(state, action: PayloadAction<{ stackId: string | null; path: string }>) {
			const { stackId, path } = action.payload;
			const prefix = partialKey(stackId, path);
			const assignments = uncommittedSelectors.hunkAssignments.selectByPrefix(
				state.hunkAssignments,
				prefix
			);

			for (const assignment of assignments) {
				const key = hunkAssignmentAdapter.selectId(assignment);
				state.hunkSelection = hunkSelectionAdapter.upsertOne(state.hunkSelection, {
					stableId: assignment.id,
					stackId: stackId,
					path: assignment.path,
					assignmentId: key,
					lines: []
				});
			}
		},
		uncheckFile(state, action: PayloadAction<{ stackId: string | null; path: string }>) {
			const { stackId, path } = action.payload;
			const prefix = partialKey(stackId, path);
			const selections = uncommittedSelectors.hunkSelection.selectByPrefix(
				state.hunkSelection,
				prefix
			);
			state.hunkSelection = hunkSelectionAdapter.removeMany(
				state.hunkSelection,
				selections.map((a) => a.assignmentId)
			);
		},
		checkDir(state, action: PayloadAction<{ stackId: string | null; path: string }>) {
			const { stackId, path } = action.payload;
			const prefix = prefixKey(stackId, path);
			const assignments = uncommittedSelectors.hunkAssignments.selectByPrefix(
				state.hunkAssignments,
				prefix
			);

			for (const assignment of assignments) {
				const key = hunkAssignmentAdapter.selectId(assignment);
				state.hunkSelection = hunkSelectionAdapter.upsertOne(state.hunkSelection, {
					stableId: assignment.id,
					stackId: stackId,
					path: assignment.path,
					assignmentId: key,
					lines: []
				});
			}
		},
		uncheckDir(state, action: PayloadAction<{ stackId: string | null; path: string }>) {
			const { stackId, path } = action.payload;
			const prefix = prefixKey(stackId, path);
			const selections = uncommittedSelectors.hunkSelection.selectByPrefix(
				state.hunkSelection,
				prefix
			);
			state.hunkSelection = hunkSelectionAdapter.removeMany(
				state.hunkSelection,
				selections.map((a) => a.assignmentId)
			);
		},
		checkStack(state, action: PayloadAction<{ stackId: string | null }>) {
			const { stackId } = action.payload;
			const prefix = partialKey(stackId);
			const assignments = uncommittedSelectors.hunkAssignments.selectByPrefix(
				state.hunkAssignments,
				prefix
			);

			for (const assignment of assignments) {
				const key = hunkAssignmentAdapter.selectId(assignment);
				state.hunkSelection = hunkSelectionAdapter.upsertOne(state.hunkSelection, {
					stableId: assignment.id,
					stackId: stackId,
					path: assignment.path,
					assignmentId: key,
					lines: []
				});
			}
		},
		uncheckStack(state, action: PayloadAction<{ stackId: string | null }>) {
			const { stackId } = action.payload;
			const prefix = partialKey(stackId);
			const selections = uncommittedSelectors.hunkSelection.selectByPrefix(
				state.hunkSelection,
				prefix
			);
			state.hunkSelection = hunkSelectionAdapter.removeMany(
				state.hunkSelection,
				selections.map((s) => s.assignmentId)
			);
		}
	}
});

/** This type is needed for `createSelector` calls. */
type AssignmentState = ReturnType<typeof uncommittedSlice.getInitialState>;

/** Dispatchable actions to mutate selection states. */
export const uncommittedActions = uncommittedSlice.actions;

/** For use in custom selectors declared below. */
function selectSelf(state: ReturnType<typeof uncommittedSlice.getInitialState>) {
	return state;
}

/** Used as input selector for several selectors below. */
const selectHunkAssignments = createSelector(
	[selectSelf],
	(rootState) => rootState.hunkAssignments
);

/** Used as input selector for selector below. */
const selectTreeChanges = createSelector([selectSelf], (rootState) => rootState.treeChanges);
const selectHunkSelection = createSelector([selectSelf], (rootState) => rootState.hunkSelection);

/**
 * Changes describe a modification to a file, and can overlap across stacks.
 * Note that a null stack id returns unassigned changes.
 */
const selectByStackId = createSelector(
	[
		selectTreeChanges,
		selectHunkAssignments,
		(_: AssignmentState, stackId: string | null) => stackId
	],
	(changes, assignments, stackId) => {
		const paths = new Set(
			Object.values(assignments.entities)
				.filter((a) => a.stackId === stackId)
				.filter(isDefined)
				.map((a) => a.path)
		);
		return changes.ids.map((id) => changes.entities[id]!).filter((c) => paths.has(c.path));
	}
);

/**
 * Changes filtered by what hunks are checked.
 */
const selectedByStackId = createSelector(
	[selectHunkSelection, selectByStackId, (_: AssignmentState, stackId: string | null) => stackId],
	(selections, changes, stackId) =>
		changes.filter((change) =>
			selections.ids.some((id) => id.startsWith(prefixKey(stackId, change.path)))
		)
);

/** Selects the tree change for a specific path. */
const selectByPath = createSelector(
	[selectSelf, (_, path: string) => path],
	(rootState, path: string) => rootState.treeChanges.entities[path]
);

const hunkCheckStatus = createSelector(
	[
		selectHunkSelection,
		(_, hunkId: { stackId: string | null; path: string; hunkHeader: HunkHeader }) => {
			return hunkId;
		}
	],
	(selections, { stackId, path, hunkHeader }) => {
		const selection = selections.entities[compositeKey({ stackId, path, hunkHeader })];
		if (!selection) {
			return { selected: false, lines: [] };
		} else {
			return { selected: true, lines: selection.lines };
		}
	}
);

export type CheckboxStatus = 'checked' | 'indeterminate' | 'unchecked';

const fileCheckStatus = createSelector(
	[
		selectHunkSelection,
		selectHunkAssignments,
		(_, args: { stackId: string | null; path: string }) => {
			return args;
		}
	],
	(selections, assignments, { stackId, path }) => {
		const prefix = partialKey(stackId, path);
		const selection = uncommittedSelectors.hunkSelection.selectByPrefix(selections, prefix);
		const stackAssignments = uncommittedSelectors.hunkAssignments.selectByPrefix(
			assignments,
			prefix
		);
		if (!selection || selection.length === 0) {
			return 'unchecked';
		} else if (
			selection.length === stackAssignments.length &&
			selection.every((s) => s.lines.length === 0)
		) {
			return 'checked';
		} else {
			return 'indeterminate';
		}
	}
);

const folderCheckStatus = createSelector(
	[
		selectHunkSelection,
		selectHunkAssignments,
		(_, args: { stackId: string | null; path: string }) => {
			return args;
		}
	],
	(selections, assignments, { stackId, path }) => {
		const prefix = prefixKey(stackId, path);
		const matches = uncommittedSelectors.hunkAssignments.selectByPrefix(assignments, prefix);
		if (matches.length === 0) {
			return 'unchecked';
		} else if (matches.every((a) => compositeKey(a) in selections.entities)) {
			return 'checked';
		} else if (matches.some((a) => compositeKey(a) in selections.entities)) {
			return 'indeterminate';
		}
		return 'unchecked';
	}
);

const stackCheckStatus = createSelector(
	[
		selectHunkSelection,
		selectHunkAssignments,
		(_, args: { stackId: string | null }) => {
			return args;
		}
	],
	(selections, assignments, { stackId }) => {
		const prefix = partialKey(stackId);
		const matches = uncommittedSelectors.hunkAssignments.selectByPrefix(assignments, prefix);
		if (matches.length === 0) {
			return 'unchecked';
		} else if (matches.every((a) => compositeKey(a) in selections.entities)) {
			return 'checked';
		} else if (matches.some((a) => compositeKey(a) in selections.entities)) {
			return 'indeterminate';
		}
		return 'unchecked';
	}
);

/**
 * All reads from the uncommitted redux slice should be included in this
 * exported object. Exporting each thing individually makes things hard to
 * keep track of, and makes naming individual selectors difficult.
 */
export const uncommittedSelectors = {
	treeChanges: {
		...treeChangeAdapter.getSelectors(),
		selectByIds: createSelectByIds<TreeChange>(),
		selectByPath,
		selectByStackId,
		selectedByStackId
	},
	hunkAssignments: {
		...hunkAssignmentAdapter.getSelectors(),
		selectByPrefix: createSelectByPrefix<HunkAssignment>(),
		selectNotIn: createSelectNotIn<HunkAssignment>()
	},
	hunkSelection: {
		...hunkSelectionAdapter.getSelectors(),
		selectByPrefix: createSelectByPrefix<HunkSelection>(),
		selectNotIn: createSelectNotIn<HunkSelection>(),
		hunkCheckStatus,
		fileCheckStatus,
		folderCheckStatus,
		stackCheckStatus
	}
};

/**
 * Replaces the old tree changes and hunk assignments entirly.
 * Then for the selections, it will loop over the old selections and:
 * - If there is a new assignment with the same stable ID, it will add the
 *   assignment, with updated header information.
 * - Otherwise, it will just be dropped.
 */
function updateAssignments(
	state: UncommittedState,
	action: PayloadAction<{ assignments: HunkAssignment[]; changes: TreeChange[] }>
): UncommittedState {
	// Read: Replace whole tree changes slice with the new changes.
	state.treeChanges = treeChangeAdapter.addMany(
		treeChangeAdapter.getInitialState(),
		action.payload.changes
	);
	state.hunkAssignments = hunkAssignmentAdapter.addMany(
		hunkAssignmentAdapter.getInitialState(),
		action.payload.assignments
	);
	const oldSelections = uncommittedSelectors.hunkSelection.selectAll(state.hunkSelection);
	// Set hunk selection to empty. We will re-build this.
	state.hunkSelection = hunkSelectionAdapter.removeAll(state.hunkSelection);

	// Keyed by stable ID or fallback to composite key.
	const newAssignments = new Map(
		action.payload.assignments.map((a) => [a.id || compositeKey(a), a])
	);

	for (const old of oldSelections) {
		const newAssignment = newAssignments.get(old.stableId || old.assignmentId);
		if (newAssignment) {
			state.hunkSelection = hunkSelectionAdapter.addOne(state.hunkSelection, {
				stableId: newAssignment.id,
				assignmentId: compositeKey(newAssignment),
				stackId: newAssignment.stackId,
				path: newAssignment.path,
				// TODO: Carry over lines from the old selection.
				lines: []
			});
		}
	}

	return state;
}
