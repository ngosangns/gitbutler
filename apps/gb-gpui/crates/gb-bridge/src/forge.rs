//! In-process pull-request picture.
//!
//! A connected fake keeps reviews in memory. [`DisconnectedForge`] answers
//! with a status sentence and does not open a project, read a token, or
//! contact a host. The upstream client names stay out of this crate so the
//! name-scan allowlist can stay empty.

use crate::{BridgeError, Result};

/// Shown when this build has no forge account.
pub const FORGE_DISCONNECTED: &str = "Forge integration is not connected in this build";

/// Tooltip and error when a draft cannot be opened.
pub const FORGE_SIGN_IN: &str = "Forge sign-in is not available in this build";

/// Pull-request list for one project.
///
/// `connected` is false when this build will not open a draft. `detail` is the
/// sentence the window shows in that case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeStatus {
    pub connected: bool,
    pub detail: String,
    pub reviews: Vec<Review>,
}

impl ForgeStatus {
    pub fn disconnected() -> Self {
        Self {
            connected: false,
            detail: FORGE_DISCONNECTED.to_string(),
            reviews: Vec::new(),
        }
    }
}

/// One pull request. Numbers and check states are canned in the fake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    pub number: u64,
    pub title: String,
    pub branch: String,
    pub state: ReviewState,
    pub ci: CiState,
    pub draft: bool,
}

/// Where the pull request sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewState {
    Open,
    Draft,
    Merged,
    Closed,
}

impl ReviewState {
    pub fn label(self) -> &'static str {
        match self {
            ReviewState::Open => "Open",
            ReviewState::Draft => "Draft",
            ReviewState::Merged => "Merged",
            ReviewState::Closed => "Closed",
        }
    }
}

/// Check result drawn as the colored dot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiState {
    None,
    Pending,
    Passing,
    Failing,
}

impl CiState {
    pub fn label(self) -> &'static str {
        match self {
            CiState::None => "No checks",
            CiState::Pending => "Checks running",
            CiState::Passing => "Checks passing",
            CiState::Failing => "Checks failing",
        }
    }
}

/// Review list, the review on one branch, and opening a draft.
///
/// Implementations stay inside this process. Nothing here dials a forge.
pub trait Forge: Send + Sync {
    fn reviews(&self, id: &str) -> Result<ForgeStatus>;
    fn review_for_branch(&self, id: &str, branch: &str) -> Result<Option<Review>>;
    fn publish_draft(&self, id: &str, branch: &str) -> Result<Review>;
}

/// Live answer. Every method returns a sentence and leaves the machine alone.
#[derive(Debug, Default, Clone, Copy)]
pub struct DisconnectedForge;

impl Forge for DisconnectedForge {
    fn reviews(&self, _id: &str) -> Result<ForgeStatus> {
        Ok(ForgeStatus::disconnected())
    }

    fn review_for_branch(&self, _id: &str, _branch: &str) -> Result<Option<Review>> {
        Err(BridgeError::not_ready(FORGE_DISCONNECTED))
    }

    fn publish_draft(&self, _id: &str, _branch: &str) -> Result<Review> {
        Err(BridgeError::not_ready(FORGE_SIGN_IN))
    }
}

/// `refs/heads/lane-a` and `lane-a` are the same branch.
pub(crate) fn normalize_branch(name: &str) -> String {
    let name = name.trim();
    name.strip_prefix("refs/heads/")
        .unwrap_or(name)
        .trim()
        .to_string()
}

pub(crate) fn find_review(reviews: &[Review], branch: &str) -> Option<Review> {
    let branch = normalize_branch(branch);
    reviews
        .iter()
        .find(|review| review.branch == branch)
        .cloned()
}

/// Insert a draft, or return the review this branch already has.
pub(crate) fn insert_draft(
    reviews: &mut Vec<Review>,
    next_number: &mut u64,
    branch: &str,
) -> Review {
    let branch = normalize_branch(branch);
    if let Some(existing) = reviews.iter().find(|review| review.branch == branch) {
        return existing.clone();
    }
    let floor = reviews
        .iter()
        .map(|review| review.number)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    let number = (*next_number).max(floor);
    *next_number = number.saturating_add(1);
    let review = Review {
        number,
        title: format!("Draft for {branch}"),
        branch,
        state: ReviewState::Draft,
        ci: CiState::Pending,
        draft: true,
    };
    reviews.push(review.clone());
    review
}

/// Canned reviews for the demo project. `lane-c` is absent so a draft can be opened.
pub(crate) fn demo_reviews() -> Vec<Review> {
    vec![
        Review {
            number: 128,
            title: "Wire the lane header".to_string(),
            branch: "lane-a".to_string(),
            state: ReviewState::Open,
            ci: CiState::Passing,
            draft: false,
        },
        Review {
            number: 131,
            title: "Drop the old helper".to_string(),
            branch: "lane-b".to_string(),
            state: ReviewState::Open,
            ci: CiState::Failing,
            draft: false,
        },
        Review {
            number: 90,
            title: "Notes for the review".to_string(),
            branch: "topic".to_string(),
            state: ReviewState::Merged,
            ci: CiState::Passing,
            draft: false,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnected_forge_returns_a_status_and_makes_no_request() {
        let forge = DisconnectedForge;
        let status = forge.reviews("demo-project").unwrap();
        assert!(!status.connected);
        assert_eq!(status.detail, FORGE_DISCONNECTED);
        assert!(status.reviews.is_empty());
        let missing = forge
            .review_for_branch("demo-project", "lane-a")
            .unwrap_err();
        assert_eq!(missing.message, FORGE_DISCONNECTED);
        let refused = forge.publish_draft("demo-project", "lane-a").unwrap_err();
        assert_eq!(refused.message, FORGE_SIGN_IN);
    }

    #[test]
    fn a_second_draft_for_the_same_branch_is_the_first_one() {
        let mut reviews = demo_reviews();
        let mut next = 132;
        let created = insert_draft(&mut reviews, &mut next, "refs/heads/lane-c");
        assert_eq!(created.number, 132);
        assert!(created.draft);
        assert_eq!(created.state, ReviewState::Draft);
        assert_eq!(created.ci, CiState::Pending);
        let again = insert_draft(&mut reviews, &mut next, "lane-c");
        assert_eq!(again.number, 132);
        assert_eq!(
            reviews
                .iter()
                .filter(|review| review.branch == "lane-c")
                .count(),
            1
        );
        assert!(find_review(&reviews, "lane-a").is_some());
        assert!(find_review(&reviews, "lane-c").is_some());
    }
}
