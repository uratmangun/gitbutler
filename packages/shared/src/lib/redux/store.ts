import { feedsReducer } from '$lib/feeds/feedsSlice';
import { postsReducer } from '$lib/feeds/postsSlice';
import { exampleReducer } from '$lib/redux/example';
import { configureStore, createSelector } from '@reduxjs/toolkit';
import { derived, readable, type Readable } from 'svelte/store';

// Individual interfaces to be used when consuming in other servies.
// By specifying only the interfaces you need, IE:
// `appState: AppPostState & AppExampleState`, it means there is less mocking
// needed when testing.
export interface AppExampleState {
	readonly example: Readable<ReturnType<typeof exampleReducer>>;
}

export interface AppPostsState {
	readonly posts: Readable<ReturnType<typeof postsReducer>>;
}

export interface AppFeedsState {
	readonly feeds: Readable<ReturnType<typeof feedsReducer>>;
}

export class AppDispatch {
	constructor(readonly dispatch: Dispatch) {}
}

export class AppState implements AppExampleState, AppPostsState, AppFeedsState {
	/**
	 * The base store.
	 *
	 * This is a low level API and should not be used directly.
	 * @private
	 */
	readonly _store = configureStore({
		reducer: {
			examples: exampleReducer,
			posts: postsReducer,
			feeds: feedsReducer
		}
	});

	readonly appDispatch = new AppDispatch(this._store.dispatch);

	/**
	 * Used to access the store directly. It is recommended to access state via
	 * selectors as they are more efficient.
	 */
	readonly rootState: Readable<RootState> = readable(this._store.getState(), (set) => {
		const unsubscribe = this._store.subscribe(() => {
			set(this._store.getState());
		});
		return unsubscribe;
	});

	private selectSelf(state: RootState) {
		return state;
	}

	private readonly selectExample = createSelector(
		[this.selectSelf],
		(rootState) => rootState.examples
	);
	readonly example = derived(this.rootState, this.selectExample);

	private readonly selectPosts = createSelector([this.selectSelf], (rootState) => rootState.posts);
	readonly posts = derived(this.rootState, this.selectPosts);

	private readonly selectFeeds = createSelector([this.selectSelf], (rootState) => rootState.feeds);
	readonly feeds = derived(this.rootState, this.selectFeeds);
}

export type RootState = ReturnType<typeof AppState.prototype._store.getState>;
export type Dispatch = typeof AppState.prototype._store.dispatch;
