//! Keyboard catalog shared by the help card and the store test.
//!
//! Each `tokens` entry is a value [`super::Store::apply_key`] accepts. A row
//! with an empty token list is help text for a key that already appears above
//! in another context.

#[derive(Debug, Clone, Copy)]
pub struct Shortcut {
    pub tokens: &'static [&'static str],
    pub keys: &'static str,
    pub what: &'static str,
}

pub fn shortcut_rows() -> &'static [Shortcut] {
    &[
        Shortcut {
            tokens: &["up", "down"],
            keys: "↑ ↓",
            what: "Move in the focused list, menu, or upstream panel",
        },
        Shortcut {
            tokens: &["left", "right", "tab", "shift-tab"],
            keys: "← → or Tab",
            what: "Move between the three columns. On the branches page, Tab opens Remote and Pull requests",
        },
        Shortcut {
            tokens: &["enter"],
            keys: "Enter",
            what: "Open the file, branch, or commit",
        },
        Shortcut {
            tokens: &["escape"],
            keys: "Esc",
            what: "Close Settings, a confirm, a menu, or a password prompt. From Branches or History, return to the workspace",
        },
        Shortcut {
            tokens: &["b"],
            keys: "b",
            what: "Open the branches page. Escape returns to the workspace",
        },
        Shortcut {
            tokens: &["h"],
            keys: "h",
            what: "Open snapshot history. Escape returns to the workspace",
        },
        Shortcut {
            tokens: &["e"],
            keys: "e",
            what: "Edit the focused commit",
        },
        Shortcut {
            tokens: &["o"],
            keys: "o",
            what: "Choose ours while resolving a conflict. Left does the same there",
        },
        Shortcut {
            tokens: &["t"],
            keys: "t",
            what: "Switch list and tree. While resolving a conflict, choose theirs",
        },
        Shortcut {
            tokens: &["cmd-shift-f"],
            keys: "⌘ Shift F",
            what: "Fetch remotes",
        },
        Shortcut {
            tokens: &["m"],
            keys: "m",
            what: "Move the file, hunk, or commit",
        },
        Shortcut {
            tokens: &["space"],
            keys: "Space",
            what: "Select a file or hunk",
        },
        Shortcut {
            tokens: &["d", "backspace"],
            keys: "d or Backspace",
            what: "Discard the file or hunk. On the branches page, Backspace edits the filter",
        },
        Shortcut {
            tokens: &["c"],
            keys: "c",
            what: "Open or close the commit editor",
        },
        Shortcut {
            tokens: &["g"],
            keys: "g",
            what: "Suggest a commit message for the selected files",
        },
        Shortcut {
            tokens: &[",", "cmd-comma"],
            keys: ", or ⌘,",
            what: "Open or close Settings",
        },
        Shortcut {
            tokens: &["cmd-z"],
            keys: "⌘Z",
            what: "Undo the last change",
        },
        Shortcut {
            tokens: &["r", "s", "u"],
            keys: "r s u",
            what: "Reword, squash, or uncommit the focused commit",
        },
        Shortcut {
            tokens: &["i", "a", "x"],
            keys: "i a x",
            what: "Insert a blank commit, amend, or open commit actions",
        },
        Shortcut {
            tokens: &["j", "k"],
            keys: "j k",
            what: "Next and previous hunk",
        },
        Shortcut {
            tokens: &["cmd-p"],
            keys: "⌘P",
            what: "Open the project menu",
        },
        Shortcut {
            tokens: &["cmd-r"],
            keys: "⌘R",
            what: "Refresh",
        },
        Shortcut {
            tokens: &["y", "n"],
            keys: "y n",
            what: "Confirm or cancel a question. In history, n focuses the snapshot message",
        },
        Shortcut {
            tokens: &[],
            keys: "Right-click or ⋯",
            what: "More actions for a file, hunk, commit, or branch",
        },
        Shortcut {
            tokens: &[],
            keys: "Drag column edge",
            what: "Resize the columns. Double-click the edge to reset",
        },
    ]
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use gb_bridge::{ConflictChoice, FakeBackend};

    use super::*;
    use crate::store::pages::{ConflictFile, PageAsk, PageOp};
    use crate::store::{DiffRow, Effect, Pane, RowKind, ShownFile, Store, load};

    fn ready() -> Store {
        let backend = FakeBackend::demo();
        let mut store = Store::default();
        store.select(Some("demo-project".to_string()));
        store.apply(load(&backend, store.selected.as_deref()));
        store
    }

    fn prepare(store: &mut Store, token: &str) {
        match token {
            "up" => store.set_cursor(Pane::Uncommitted, 1),
            "escape" => store.help = true,
            "e" | "r" | "u" | "i" | "a" | "x" | "s" => {
                store.focus_commit("a11a11a");
            }
            "g" => store.mark_click("src/app.rs", false),
            "j" => prepare_hunks(store, 0),
            "k" => prepare_hunks(store, 1),
            "n" => store.pages.history_open = true,
            "y" => {
                store.pages.ask = Some(PageAsk {
                    title: "Delete branch?".to_string(),
                    message: "Delete the local branch.".to_string(),
                    confirm_label: "Delete".to_string(),
                    destructive: true,
                    op: PageOp::Delete {
                        name: "lane-a".to_string(),
                    },
                });
            }
            "o" => {
                store.pages.conflicts_open = true;
                store.pages.conflict_files = vec![ConflictFile {
                    path: "src/app.rs".to_string(),
                    side: None,
                }];
            }
            _ => {}
        }
    }

    fn prepare_hunks(store: &mut Store, cursor: usize) {
        store.pane = Pane::Changes;
        store.shown = ShownFile::Worktree {
            path: "src/app.rs".to_string(),
        };
        store.diff_rows = Arc::new(vec![
            DiffRow {
                kind: RowKind::Hunk,
                old_line: None,
                new_line: None,
                text: "@@ -1 +1 @@".to_string(),
                hunk: Some(0),
                lane: None,
            },
            DiffRow {
                kind: RowKind::Hunk,
                old_line: None,
                new_line: None,
                text: "@@ -2 +2 @@".to_string(),
                hunk: Some(1),
                lane: None,
            },
        ]);
        store.hunk_cursor = cursor;
    }

    fn fingerprint(store: &Store) -> String {
        let side = store
            .pages
            .conflict_files
            .first()
            .and_then(|file| file.side)
            .map(crate::store::side_name)
            .unwrap_or("");
        format!(
            "{:?}|{:?}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{:?}|{}|{}|{}|{}|{}|{}",
            store.pane,
            store.view,
            store.cursor(Pane::Uncommitted),
            store.help,
            store.project_menu,
            store.settings.open,
            store.pages.branches_open,
            store.pages.history_open,
            store.pages.conflicts_open,
            store.marked.len(),
            store.composer.open,
            store.hunk_cursor,
            store.menu.is_some(),
            store.confirm.is_some(),
            store.toast,
            store.pages.tab.name(),
            store.settings.confirm_remove,
            store.composer.suggest_error.as_deref().unwrap_or(""),
            side,
            store.pages.message_focused,
            store.reword.is_some(),
        )
    }

    #[test]
    fn every_listed_shortcut_does_something() {
        let mut tokens = BTreeSet::new();
        let mut help = String::new();
        for row in shortcut_rows() {
            help.push_str(row.keys);
            help.push(' ');
            help.push_str(row.what);
            help.push('\n');
            for token in row.tokens {
                tokens.insert(*token);
            }
        }
        assert!(help.contains("Settings"), "{help}");
        assert!(help.contains("Suggest"), "{help}");
        assert!(help.contains("Pull requests"), "{help}");
        assert!(help.contains("More actions"), "{help}");
        assert!(help.contains("Double-click"), "{help}");
        for token in tokens {
            let mut store = ready();
            prepare(&mut store, token);
            let before = fingerprint(&store);
            let effect = store.apply_key(token);
            let after = fingerprint(&store);
            assert!(
                effect != Effect::None || before != after,
                "shortcut {token} did not change the store"
            );
        }
    }

    #[test]
    fn tab_on_the_branches_page_reaches_pull_requests() {
        let mut store = ready();
        store.pages.branches_open = true;
        store.apply_key("tab");
        store.apply_key("tab");
        assert_eq!(store.pages.tab, crate::store::BranchTab::PullRequests);
    }

    #[test]
    fn conflict_t_chooses_theirs() {
        let mut store = ready();
        store.pages.conflicts_open = true;
        store.pages.conflict_files = vec![ConflictFile {
            path: "src/app.rs".to_string(),
            side: None,
        }];
        store.apply_key("t");
        assert_eq!(
            store.pages.conflict_files[0].side,
            Some(ConflictChoice::Theirs)
        );
    }
}
