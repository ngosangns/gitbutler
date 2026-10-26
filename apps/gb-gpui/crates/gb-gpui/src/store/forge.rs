//! Pull-request chips and commit-message suggestions.
//!
//! No GPUI types. The window runs a [`ForgeOp`] or a suggest call, then
//! [`Store::accept_forge`] or [`Store::accept_suggestion`]. The fake backend
//! keeps drafts in memory. A live backend returns a sentence and is not asked
//! to sign in.

use std::sync::Arc;

use gb_bridge::{Backend, FORGE_SIGN_IN, ForgeStatus, Review, SuggestedMessage};

use super::pages::{BranchTab, move_index};
use super::{Effect, Field, Store};

/// Shown in the composer when nothing is selected.
pub const SELECT_FILES: &str = "Select a file before asking for a message.";

/// Shown when a suggestion would replace text the user already typed.
pub const REPLACE_PROMPT: &str = "Replace the title and body you already typed?";

/// One forge call the window should run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgeOp {
    Publish { branch: String },
}

/// What the bridge returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgeDone {
    Published(Review),
}

/// Reviews for the open project, plus the draft that is opening.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeUi {
    pub connected: bool,
    pub detail: String,
    pub reviews: Vec<Review>,
    pub publishing: Option<String>,
    pub cursor: usize,
}

impl Default for ForgeUi {
    fn default() -> Self {
        Self::from_status(ForgeStatus::disconnected())
    }
}

impl ForgeUi {
    fn from_status(status: ForgeStatus) -> Self {
        Self {
            connected: status.connected,
            detail: status.detail,
            reviews: status.reviews,
            publishing: None,
            cursor: 0,
        }
    }
}

impl Store {
    pub(super) fn reset_forge(&mut self) {
        self.bump_forge();
        self.forge = ForgeUi::default();
    }

    pub(super) fn apply_forge_status(&mut self, status: ForgeStatus) {
        self.bump_forge();
        let publishing = self.forge.publishing.clone();
        let cursor = self.forge.cursor;
        self.forge = ForgeUi::from_status(status);
        self.forge.publishing = publishing;
        self.forge.cursor = cursor;
        self.clamp_review_cursor();
    }

    pub fn review_for(&self, branch: &str) -> Option<&Review> {
        let branch = branch.trim().trim_start_matches("refs/heads/");
        self.forge
            .reviews
            .iter()
            .find(|review| review.branch == branch)
    }

    /// Reviews matching the committed branch filter, sorted by number.
    /// Cached per (forge generation, filter); typing debounces the filter.
    pub fn visible_reviews(&self) -> Arc<Vec<Review>> {
        let committed = self.derived.borrow().branch_filter.clone();
        let forge_gen = self.derived.borrow().forge_gen;
        let key = (forge_gen, committed);
        if let Some((have, rows)) = self.derived.borrow().reviews.as_ref()
            && *have == key
        {
            return Arc::clone(rows);
        }
        let rows = Arc::new(filtered_reviews(&self.forge.reviews, &key.1));
        self.derived.borrow_mut().reviews = Some((key, Arc::clone(&rows)));
        rows
    }

    #[cfg(any(test, feature = "devtools"))]
    pub fn selected_review(&self) -> Option<&Review> {
        let visible = self.visible_reviews();
        let cursor = self.forge.cursor;
        // The vec is temporary. Look up the same review on the stored list.
        let branch = visible.get(cursor).map(|review| review.branch.clone())?;
        self.review_for(&branch)
    }

    /// Open a draft for `branch`. A disconnected build refuses before any call.
    pub fn request_publish(&mut self, branch: &str) -> Result<Effect, String> {
        let branch = branch.trim().trim_start_matches("refs/heads/");
        if branch.is_empty() {
            return Err("Choose a branch before publishing a pull request.".to_string());
        }
        if !self.forge.connected {
            return Err(FORGE_SIGN_IN.to_string());
        }
        if self.forge.publishing.is_some() {
            return Err("Wait for the pull request to finish opening.".to_string());
        }
        if self.review_for(branch).is_some() {
            return Err(format!("{branch} already has a pull request."));
        }
        self.forge.publishing = Some(branch.to_string());
        Ok(Effect::Forge(ForgeOp::Publish {
            branch: branch.to_string(),
        }))
    }

    pub fn accept_forge(&mut self, done: ForgeDone) {
        match done {
            ForgeDone::Published(review) => {
                self.bump_forge();
                let branch = review.branch.clone();
                let message = if self.review_for(&branch).is_none() {
                    format!("Opened draft pull request #{} for {branch}.", review.number)
                } else {
                    format!(
                        "Pull request #{} for {branch} is already open.",
                        review.number
                    )
                };
                self.activity.succeed("publish_draft", message.clone());
                self.forge.publishing = None;
                if let Some(existing) = self
                    .forge
                    .reviews
                    .iter_mut()
                    .find(|item| item.branch == review.branch)
                {
                    *existing = review.clone();
                } else {
                    self.forge.reviews.push(review.clone());
                }
                self.forge.reviews.sort_by_key(|item| item.number);
                self.toast_bad = false;
                self.toast = Some(message);
                self.clamp_review_cursor();
            }
        }
    }

    pub fn fail_forge(&mut self, branch: &str, message: String) {
        self.activity.fail("publish_draft", message.clone());
        if self.forge.publishing.as_deref() == Some(branch) {
            self.forge.publishing = None;
        }
        self.toast_bad = true;
        self.toast = Some(message);
    }

    /// Show `branch` on the pull-request tab. Opens the page when it is closed.
    pub fn show_review(&mut self, branch: &str) -> Effect {
        self.pages.tab = BranchTab::PullRequests;
        if let Some(index) = self
            .visible_reviews()
            .iter()
            .position(|review| review.branch == branch)
        {
            self.forge.cursor = index;
        }
        if self.pages.branches_open {
            Effect::None
        } else {
            self.open_branches()
        }
    }

    pub fn move_review(&mut self, step: i32) {
        let len = self.visible_reviews().len();
        self.forge.cursor = move_index(len, self.forge.cursor, step);
    }

    pub fn clamp_review_cursor(&mut self) {
        let len = self.visible_reviews().len();
        if len == 0 {
            self.forge.cursor = 0;
        } else if self.forge.cursor >= len {
            self.forge.cursor = len - 1;
        }
    }

    /// Ask for a message for the files the next commit would take.
    pub fn request_suggestion(&mut self) -> Effect {
        if self.suggest_busy {
            return Effect::None;
        }
        self.ensure_composer_lane();
        let paths = self.suggestion_paths();
        if paths.is_empty() {
            self.suggest_busy = false;
            self.composer.suggest_error = Some(SELECT_FILES.to_string());
            return Effect::None;
        }
        self.composer.suggest_error = None;
        self.suggest_busy = true;
        Effect::Suggest(paths)
    }

    pub fn suggestion_paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = self
            .commit_specs()
            .iter()
            .map(|spec| spec.path.clone())
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }

    pub fn accept_suggestion(&mut self, message: SuggestedMessage) {
        self.activity
            .succeed("suggest_message", "Suggestion is ready.");
        self.suggest_busy = false;
        if self.composer_has_typed_text() {
            self.message_hold = Some(message);
            self.composer.open = true;
            self.composer.field = None;
            self.composer.suggest_error = None;
            return;
        }
        self.apply_suggestion(message);
    }

    pub fn fail_suggestion(&mut self, message: String) {
        self.activity.fail("suggest_message", message.clone());
        self.suggest_busy = false;
        self.message_hold = None;
        self.composer.suggest_error = Some(message);
    }

    pub fn replace_held_message(&mut self) {
        if let Some(message) = self.message_hold.take() {
            self.apply_suggestion(message);
        }
    }

    pub fn keep_typed_message(&mut self) {
        self.message_hold = None;
        self.composer.suggest_error = None;
    }

    fn composer_has_typed_text(&self) -> bool {
        !self.composer.title.is_empty() || !self.composer.body.is_empty()
    }

    fn apply_suggestion(&mut self, message: SuggestedMessage) {
        self.composer.title = message.title;
        self.composer.body = message.body;
        self.composer.suggest_error = None;
        self.composer.field = Some(Field::Title);
        self.message_hold = None;
        self.composer.open = true;
    }
}

pub fn perform_forge(
    backend: &dyn Backend,
    id: Option<&str>,
    op: &ForgeOp,
) -> Result<ForgeDone, String> {
    let id = id.ok_or_else(|| "Choose a project first.".to_string())?;
    match op {
        ForgeOp::Publish { branch } => {
            let review = backend
                .publish_draft(id, branch)
                .map_err(|error| error.message)?;
            Ok(ForgeDone::Published(review))
        }
    }
}

pub fn perform_suggest(
    backend: &dyn Backend,
    id: Option<&str>,
    paths: &[String],
) -> Result<SuggestedMessage, String> {
    let id = id.ok_or_else(|| "Choose a project first.".to_string())?;
    backend
        .suggest_message(id, paths)
        .map_err(|error| error.message)
}

pub fn filtered_reviews(reviews: &[Review], query: &str) -> Vec<Review> {
    let query = query.trim().to_ascii_lowercase();
    let mut rows: Vec<Review> = reviews
        .iter()
        .filter(|review| review_matches(review, &query))
        .cloned()
        .collect();
    rows.sort_by_key(|review| review.number);
    rows
}

fn review_matches(review: &Review, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    review.branch.to_ascii_lowercase().contains(query)
        || review.title.to_ascii_lowercase().contains(query)
        || review.state.label().to_ascii_lowercase().contains(query)
        || review.ci.label().to_ascii_lowercase().contains(query)
        || review.number.to_string().contains(query)
}

#[cfg(test)]
mod tests {
    use super::super::load;
    use super::*;
    use gb_bridge::{CiState, FakeBackend, ReviewState};

    fn ready() -> (FakeBackend, Store) {
        let backend = FakeBackend::demo();
        let mut store = Store::default();
        store.select(Some("demo-project".to_string()));
        store.apply(load(&backend, store.selected.as_deref()));
        (backend, store)
    }

    #[test]
    fn the_demo_lanes_show_canned_reviews_and_a_draft_sticks() {
        let (backend, mut store) = ready();
        assert!(store.forge.connected);
        let lane_a = store.review_for("lane-a").unwrap();
        assert_eq!(lane_a.number, 128);
        assert_eq!(lane_a.state, ReviewState::Open);
        assert_eq!(lane_a.ci, CiState::Passing);
        assert_eq!(store.review_for("lane-b").unwrap().ci, CiState::Failing);
        assert!(store.review_for("lane-c").is_none());
        let effect = store.request_publish("lane-c").unwrap();
        let Effect::Forge(op) = effect else {
            panic!("expected a forge call");
        };
        assert_eq!(store.forge.publishing.as_deref(), Some("lane-c"));
        let done = perform_forge(&backend, Some("demo-project"), &op).unwrap();
        store.accept_forge(done);
        let draft = store.review_for("lane-c").unwrap();
        assert_eq!(draft.number, 132);
        assert_eq!(draft.state, ReviewState::Draft);
        assert_eq!(draft.ci, CiState::Pending);
        assert!(store.forge.publishing.is_none());
        assert_eq!(
            store.toast.as_deref(),
            Some("Opened draft pull request #132 for lane-c.")
        );
        let again = backend.publish_draft("demo-project", "lane-c").unwrap();
        assert_eq!(again.number, 132);
    }

    #[test]
    fn a_disconnected_build_does_not_publish() {
        let (_backend, mut store) = ready();
        store.forge.connected = false;
        let error = store.request_publish("lane-c").unwrap_err();
        assert_eq!(error, FORGE_SIGN_IN);
        assert!(store.forge.publishing.is_none());
    }

    #[test]
    fn the_pull_request_tab_filters_and_moves() {
        let (_backend, mut store) = ready();
        store.pages.branches_open = true;
        store.set_branch_tab(BranchTab::PullRequests);
        let numbers: Vec<u64> = store
            .visible_reviews()
            .iter()
            .map(|review| review.number)
            .collect();
        assert_eq!(numbers, vec![90, 128, 131]);
        store.set_branch_filter("helper".to_string());
        assert_eq!(store.visible_reviews().len(), 1);
        assert_eq!(store.visible_reviews()[0].branch, "lane-b");
        store.set_branch_filter(String::new());
        store.move_review(1);
        assert_eq!(store.selected_review().unwrap().number, 128);
        store.move_review(-1);
        assert_eq!(store.selected_review().unwrap().number, 90);
    }

    #[test]
    fn visible_reviews_match_the_uncached_filter_and_refresh_after_publish() {
        let (backend, mut store) = ready();
        for query in ["", "helper", " 128 ", "LANE", "missing", "9"] {
            store.set_branch_filter(query.to_string());
            assert_eq!(
                *store.visible_reviews(),
                filtered_reviews(&store.forge.reviews, query),
                "{query:?}"
            );
            let once = store.visible_reviews();
            assert!(Arc::ptr_eq(&once, &store.visible_reviews()), "{query:?}");
        }
        // Publishing bumps the forge generation, so the cache picks up the draft.
        store.set_branch_filter(String::new());
        assert!(
            store
                .visible_reviews()
                .iter()
                .all(|review| review.branch != "lane-c")
        );
        let effect = store.request_publish("lane-c").unwrap();
        let Effect::Forge(op) = effect else {
            panic!("expected a forge call");
        };
        let done = perform_forge(&backend, Some("demo-project"), &op).unwrap();
        store.accept_forge(done);
        let numbers: Vec<u64> = store
            .visible_reviews()
            .iter()
            .map(|review| review.number)
            .collect();
        assert_eq!(numbers, vec![90, 128, 131, 132]);
        assert_eq!(
            *store.visible_reviews(),
            filtered_reviews(&store.forge.reviews, "")
        );
    }

    #[test]
    fn a_suggestion_fills_an_empty_composer_and_asks_before_replacing_text() {
        let (backend, mut store) = ready();
        store.set_composer_open(true);
        let effect = store.request_suggestion();
        let Effect::Suggest(paths) = effect else {
            panic!("expected a suggestion");
        };
        assert!(store.suggest_busy);
        assert!(paths.iter().any(|path| path == "README.md"));
        let message = perform_suggest(&backend, Some("demo-project"), &paths).unwrap();
        store.accept_suggestion(message.clone());
        assert!(!store.suggest_busy);
        assert_eq!(store.composer.title, message.title);
        assert_eq!(store.composer.body, message.body);
        assert!(store.message_hold.is_none());

        store.composer.title = "Keep this title".to_string();
        store.composer.body = "And this body".to_string();
        let effect = store.request_suggestion();
        let Effect::Suggest(paths) = effect else {
            panic!("expected a suggestion");
        };
        let message = perform_suggest(&backend, Some("demo-project"), &paths).unwrap();
        store.accept_suggestion(message.clone());
        assert_eq!(store.composer.title, "Keep this title");
        assert_eq!(store.composer.body, "And this body");
        assert_eq!(store.message_hold.as_ref(), Some(&message));
        store.keep_typed_message();
        assert_eq!(store.composer.title, "Keep this title");
        assert!(store.message_hold.is_none());

        store.message_hold = Some(message.clone());
        store.replace_held_message();
        assert_eq!(store.composer.title, message.title);
        assert!(store.message_hold.is_none());
    }

    #[test]
    fn an_empty_selection_and_a_failed_suggestion_stay_inline() {
        let (_backend, mut store) = ready();
        store.worktree = Some(std::sync::Arc::new(gb_bridge::WorktreeView {
            files: Vec::new(),
            changed_paths: Vec::new(),
        }));
        store.set_composer_open(true);
        assert_eq!(store.request_suggestion(), Effect::None);
        assert_eq!(store.composer.suggest_error.as_deref(), Some(SELECT_FILES));
        assert!(!store.suggest_busy);

        store.suggest_busy = true;
        store.fail_suggestion("Set up an AI provider in Settings".to_string());
        assert!(!store.suggest_busy);
        assert_eq!(
            store.composer.suggest_error.as_deref(),
            Some("Set up an AI provider in Settings")
        );
    }
}
