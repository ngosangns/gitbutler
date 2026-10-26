//! Commit-message suggestions.
//!
//! [`FakeMessageGenerator`] builds a title and a body from the file names.
//! [`UnavailableMessageGenerator`] is the live path: it returns a sentence and
//! does not contact a model.

use crate::{BridgeError, Result};

/// Shown when this build has no model configured.
pub const AI_UNAVAILABLE: &str = "Set up an AI provider in Settings";

/// Title and body the composer can apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuggestedMessage {
    pub title: String,
    pub body: String,
}

/// Turn selected paths into a commit message.
pub trait MessageGenerator: Send + Sync {
    fn suggest(&self, paths: &[String]) -> Result<SuggestedMessage>;
}

/// Same paths, in any order, always produce the same text.
#[derive(Debug, Default, Clone, Copy)]
pub struct FakeMessageGenerator;

impl MessageGenerator for FakeMessageGenerator {
    fn suggest(&self, paths: &[String]) -> Result<SuggestedMessage> {
        Ok(suggest_from_names(paths))
    }
}

/// Live generator. The paths are ignored. No request is made.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableMessageGenerator;

impl MessageGenerator for UnavailableMessageGenerator {
    fn suggest(&self, _paths: &[String]) -> Result<SuggestedMessage> {
        Err(BridgeError::unavailable(AI_UNAVAILABLE))
    }
}

pub(crate) fn suggest_from_names(paths: &[String]) -> SuggestedMessage {
    let mut names: Vec<&str> = paths
        .iter()
        .map(String::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .collect();
    names.sort_unstable();
    names.dedup();
    if names.is_empty() {
        return SuggestedMessage {
            title: "Update the workspace".to_string(),
            body: "No files were selected.".to_string(),
        };
    }
    let bases: Vec<&str> = names.iter().copied().map(file_base).collect();
    let title = match bases.as_slice() {
        [] => "Update the workspace".to_string(),
        [one] => format!("Update {one}"),
        [first, second] => format!("Update {first} and {second}"),
        [first, second, rest @ ..] => {
            format!("Update {first}, {second}, and {} more", rest.len())
        }
    };
    let mut body = String::from("Selected files:\n");
    for name in &names {
        body.push_str("- ");
        body.push_str(name);
        body.push('\n');
    }
    body.pop();
    SuggestedMessage { title, body }
}

fn file_base(path: &str) -> &str {
    path.rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fake_message_follows_the_file_names() {
        let generator = FakeMessageGenerator;
        let left = generator
            .suggest(&[
                "src/b.rs".to_string(),
                "README.md".to_string(),
                "src/b.rs".to_string(),
            ])
            .unwrap();
        let right = generator
            .suggest(&["README.md".to_string(), "src/b.rs".to_string()])
            .unwrap();
        assert_eq!(left, right);
        assert_eq!(left.title, "Update README.md and b.rs");
        assert_eq!(left.body, "Selected files:\n- README.md\n- src/b.rs");
        let three = generator
            .suggest(&[
                "src/split.rs".to_string(),
                "README.md".to_string(),
                "src/app.rs".to_string(),
            ])
            .unwrap();
        assert_eq!(three.title, "Update README.md, app.rs, and 1 more");
        let empty = generator.suggest(&[]).unwrap();
        assert_eq!(empty.title, "Update the workspace");
        assert_eq!(empty.body, "No files were selected.");
    }

    #[test]
    fn the_live_generator_is_unavailable() {
        let error = UnavailableMessageGenerator
            .suggest(&["README.md".to_string()])
            .unwrap_err();
        assert_eq!(error.message, AI_UNAVAILABLE);
        assert_eq!(error.kind, crate::BridgeErrorKind::NotReady);
    }
}
