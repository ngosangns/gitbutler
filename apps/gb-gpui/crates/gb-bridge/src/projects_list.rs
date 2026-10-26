//! List-only project removal.
//!
//! Upstream `gitbutler_project::delete` also removes the project's metadata
//! directory, `<repo>/.git/gitbutler` and the `gitbutler/*` refs. This module
//! only rewrites `projects.json` after a verified backup. It never removes a
//! file, a directory or a ref, and it refuses to write while the GitButler
//! desktop app runs (the app rewrites the same file).

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::{BridgeError, Result};

const PROJECTS_FILE: &str = "projects.json";
/// Executable names of the desktop app (stable, nightly, dev). Compared against
/// the process name only, so a JVM with `gitbutler` in its class path is ignored.
const APP_NAMES: [&str; 4] = [
    "gitbutler",
    "gitbutler-tauri",
    "gitbutler nightly",
    "gitbutler dev",
];

/// Reports whether the GitButler desktop app is running.
pub trait AppProbe {
    fn app_running(&self) -> bool;
}

/// Looks at the process table with `ps`.
pub struct PsProbe;

impl AppProbe for PsProbe {
    fn app_running(&self) -> bool {
        let Ok(output) = Command::new("ps").args(["-axo", "comm="]).output() else {
            // Unknown state counts as running: refuse rather than guess.
            return true;
        };
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| is_app_process(line.trim()))
    }
}

fn is_app_process(command: &str) -> bool {
    let name = command.rsplit('/').next().unwrap_or(command).to_lowercase();
    APP_NAMES.contains(&name.as_str())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectEntry {
    pub id: String,
    pub title: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveReport {
    pub removed: Vec<ProjectEntry>,
    pub kept: Vec<ProjectEntry>,
    /// `None` for a dry run.
    pub backup_path: Option<PathBuf>,
    pub dry_run: bool,
}

/// Which entries leave the list.
#[derive(Debug, Clone)]
pub enum Selector<'a> {
    Ids(&'a [String]),
    /// Everything except entries whose `path` is listed.
    AllExceptPaths(&'a [String]),
}

pub fn default_backup_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(".agent-scratch/gb-gpui/backups")
}

fn error(message: impl Into<String>) -> BridgeError {
    BridgeError::internal(message)
}

fn entries(list: &[Value]) -> Vec<ProjectEntry> {
    list.iter()
        .map(|value| {
            let text = |key: &str| {
                value
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            ProjectEntry {
                id: text("id"),
                title: text("title"),
                path: text("path"),
            }
        })
        .collect()
}

fn read_list(file: &Path) -> Result<Vec<Value>> {
    let bytes =
        fs::read(file).map_err(|e| error(format!("Could not read {}: {e}", file.display())))?;
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Array(list)) => Ok(list),
        Ok(_) => Err(error(format!("{} is not a project list", file.display()))),
        Err(e) => Err(error(format!("{} is not valid JSON: {e}", file.display()))),
    }
}

fn guard(probe: &dyn AppProbe) -> Result<()> {
    if probe.app_running() {
        return Err(BridgeError::not_exclusive(
            "GitButler is running, so its project list is locked.",
        )
        .with_hint("Quit the GitButler app and try again. Nothing was changed."));
    }
    Ok(())
}

fn stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{secs}-{nanos:09}")
}

/// Copies `source` into `backup_dir` (created if missing) and checks that the
/// copy is byte-identical. Existing backups are never overwritten.
fn backup(source: &Path, backup_dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(backup_dir)
        .map_err(|e| error(format!("Could not create the backup folder: {e}")))?;
    let target = backup_dir.join(format!("{PROJECTS_FILE}.{}", stamp()));
    let original =
        fs::read(source).map_err(|e| error(format!("Could not read the project list: {e}")))?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)
        .map_err(|e| error(format!("Could not create the backup: {e}")))?;
    file.write_all(&original)
        .and_then(|()| file.sync_all())
        .map_err(|e| error(format!("Could not write the backup: {e}")))?;
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        // The list holds local paths: keep the copy private like the original.
        let _ = fs::set_permissions(&target, fs::Permissions::from_mode(0o600));
    }
    let copy = fs::read(&target).map_err(|e| error(format!("Could not verify the backup: {e}")))?;
    if copy != original {
        return Err(error(
            "The backup did not match the original. Nothing was changed.",
        ));
    }
    Ok(target)
}

/// Atomic replace: temp file next to the target, fsync, rename. Keeps the
/// existing file mode (GitButler writes 0600).
fn write_list(file: &Path, list: &[Value]) -> Result<()> {
    let dir = file
        .parent()
        .ok_or_else(|| error("Project list has no folder"))?;
    let mode = fs::metadata(file).map(|m| m.permissions()).ok();
    let temp = dir.join(format!(".{PROJECTS_FILE}.{}.tmp", stamp()));
    let mut text = serde_json::to_string_pretty(list)
        .map_err(|e| error(format!("Could not encode the project list: {e}")))?;
    text.push('\n');
    let result = (|| -> std::io::Result<()> {
        let mut out = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        out.write_all(text.as_bytes())?;
        out.sync_all()?;
        if let Some(mode) = mode {
            fs::set_permissions(&temp, mode)?;
        }
        fs::rename(&temp, file)
    })();
    if let Err(e) = result {
        // Only our own temp file can be left behind; clean it so no stray file remains.
        let _ = fs::remove_file(&temp);
        return Err(error(format!("Could not save the project list: {e}")));
    }
    Ok(())
}

/// Removes entries from `<app_data_dir>/projects.json`. See the module docs.
pub fn remove_projects_from_list(
    app_data_dir: &Path,
    selector: Selector<'_>,
    backup_dir: &Path,
    probe: &dyn AppProbe,
    dry_run: bool,
) -> Result<RemoveReport> {
    let file = app_data_dir.join(PROJECTS_FILE);
    let list = read_list(&file)?;
    let all = entries(&list);
    let (mut removed, mut kept, mut keep_values) = (Vec::new(), Vec::new(), Vec::new());
    for (entry, value) in all.into_iter().zip(list) {
        let hit = match &selector {
            Selector::Ids(ids) => ids.contains(&entry.id),
            Selector::AllExceptPaths(paths) => !paths.contains(&entry.path),
        };
        if hit {
            removed.push(entry);
        } else {
            kept.push(entry);
            keep_values.push(value);
        }
    }
    if dry_run || removed.is_empty() {
        return Ok(RemoveReport {
            removed,
            kept,
            backup_path: None,
            dry_run,
        });
    }
    guard(probe)?;
    let backup_path = backup(&file, backup_dir)?;
    write_list(&file, &keep_values)?;
    Ok(RemoveReport {
        removed,
        kept,
        backup_path: Some(backup_path),
        dry_run: false,
    })
}

/// Puts a backup made by [`remove_projects_from_list`] back. The current file
/// is backed up first.
pub fn restore_projects_from_backup(
    app_data_dir: &Path,
    backup_file: &Path,
    backup_dir: &Path,
    probe: &dyn AppProbe,
) -> Result<PathBuf> {
    let list = read_list(backup_file)?;
    guard(probe)?;
    let file = app_data_dir.join(PROJECTS_FILE);
    let current_backup = if file.exists() {
        backup(&file, backup_dir)?
    } else {
        PathBuf::new()
    };
    write_list(&file, &list)?;
    Ok(current_backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Probe(bool);
    impl AppProbe for Probe {
        fn app_running(&self) -> bool {
            self.0
        }
    }

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let backups = root.path().join("backups");
        fs::create_dir_all(&data).unwrap();
        let repos: Vec<Value> = ["keep", "drop1", "drop2"]
            .iter()
            .map(|name| {
                let repo = root.path().join(name);
                fs::create_dir_all(repo.join(".git/gitbutler")).unwrap();
                fs::write(repo.join(".git/gitbutler/marker"), "x").unwrap();
                serde_json::json!({
                    "id": format!("id-{name}"),
                    "title": name,
                    "path": repo.to_string_lossy(),
                    "extra": {"nested": [1, 2, 3]},
                })
            })
            .collect();
        let file = data.join(PROJECTS_FILE);
        fs::write(&file, serde_json::to_vec(&repos).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        }
        (root, data, backups)
    }

    fn tree(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                out.push(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
                if path.is_dir() {
                    walk(&path, root, out);
                }
            }
        }
        walk(root, root, &mut out);
        out.sort();
        out
    }

    #[test]
    fn removes_only_listed_ids_and_keeps_other_fields() {
        let (root, data, backups) = fixture();
        let before = fs::read(data.join(PROJECTS_FILE)).unwrap();
        let ids = vec!["id-drop1".to_string()];
        let report =
            remove_projects_from_list(&data, Selector::Ids(&ids), &backups, &Probe(false), false)
                .unwrap();
        assert_eq!(report.removed.len(), 1);
        assert_eq!(report.kept.len(), 2);
        let after = read_list(&data.join(PROJECTS_FILE)).unwrap();
        assert_eq!(after.len(), 2);
        assert_eq!(after[0]["extra"]["nested"], serde_json::json!([1, 2, 3]));
        assert_eq!(fs::read(report.backup_path.unwrap()).unwrap(), before);
        drop(root);
    }

    #[test]
    fn all_except_keeps_current_path_and_touches_no_directory() {
        let (root, data, backups) = fixture();
        let dirs_before: Vec<_> = tree(root.path())
            .into_iter()
            .filter(|p| !p.starts_with("backups") && !p.starts_with("data"))
            .collect();
        let keep = vec![root.path().join("keep").to_string_lossy().into_owned()];
        let report = remove_projects_from_list(
            &data,
            Selector::AllExceptPaths(&keep),
            &backups,
            &Probe(false),
            false,
        )
        .unwrap();
        assert_eq!(report.removed.len(), 2);
        let dirs_after: Vec<_> = tree(root.path())
            .into_iter()
            .filter(|p| !p.starts_with("backups") && !p.starts_with("data"))
            .collect();
        assert_eq!(dirs_before, dirs_after);
        assert!(root.path().join("drop1/.git/gitbutler/marker").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(data.join(PROJECTS_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // No temp file left in the data dir.
        assert_eq!(tree(&data), vec![PROJECTS_FILE.to_string()]);
    }

    #[test]
    fn refuses_while_app_runs_and_writes_nothing() {
        let (_root, data, backups) = fixture();
        let before = fs::read(data.join(PROJECTS_FILE)).unwrap();
        let ids = vec!["id-drop1".to_string()];
        let err =
            remove_projects_from_list(&data, Selector::Ids(&ids), &backups, &Probe(true), false)
                .unwrap_err();
        assert_eq!(err.kind, crate::BridgeErrorKind::NotExclusive);
        assert_eq!(fs::read(data.join(PROJECTS_FILE)).unwrap(), before);
        assert!(!backups.exists());
    }

    #[test]
    fn refuses_when_backup_fails() {
        let (root, data, _backups) = fixture();
        let blocker = root.path().join("not-a-dir");
        fs::write(&blocker, "file").unwrap();
        let before = fs::read(data.join(PROJECTS_FILE)).unwrap();
        let ids = vec!["id-drop1".to_string()];
        let result = remove_projects_from_list(
            &data,
            Selector::Ids(&ids),
            &blocker.join("sub"),
            &Probe(false),
            false,
        );
        assert!(result.is_err());
        assert_eq!(fs::read(data.join(PROJECTS_FILE)).unwrap(), before);
    }

    #[test]
    fn dry_run_and_unknown_ids_write_nothing() {
        let (_root, data, backups) = fixture();
        let before = fs::read(data.join(PROJECTS_FILE)).unwrap();
        let ids = vec!["id-drop1".to_string()];
        let dry =
            remove_projects_from_list(&data, Selector::Ids(&ids), &backups, &Probe(true), true)
                .unwrap();
        assert!(dry.dry_run && dry.backup_path.is_none() && dry.removed.len() == 1);
        let unknown = vec!["nope".to_string()];
        let none = remove_projects_from_list(
            &data,
            Selector::Ids(&unknown),
            &backups,
            &Probe(false),
            false,
        )
        .unwrap();
        assert!(none.removed.is_empty() && none.backup_path.is_none());
        assert_eq!(fs::read(data.join(PROJECTS_FILE)).unwrap(), before);
        assert!(!backups.exists());
    }

    #[test]
    fn restore_round_trip() {
        let (_root, data, backups) = fixture();
        let before = read_list(&data.join(PROJECTS_FILE)).unwrap();
        let keep = vec![String::new()];
        let report = remove_projects_from_list(
            &data,
            Selector::AllExceptPaths(&keep),
            &backups,
            &Probe(false),
            false,
        )
        .unwrap();
        assert!(read_list(&data.join(PROJECTS_FILE)).unwrap().is_empty());
        let backup_file = report.backup_path.unwrap();
        assert!(restore_projects_from_backup(&data, &backup_file, &backups, &Probe(true)).is_err());
        restore_projects_from_backup(&data, &backup_file, &backups, &Probe(false)).unwrap();
        assert_eq!(read_list(&data.join(PROJECTS_FILE)).unwrap(), before);
    }

    #[test]
    fn app_name_matching_ignores_jvm_with_gitbutler_in_classpath() {
        assert!(is_app_process(
            "/Applications/GitButler.app/Contents/MacOS/gitbutler-tauri"
        ));
        assert!(is_app_process(
            "/Applications/GitButler Nightly.app/Contents/MacOS/GitButler Nightly"
        ));
        assert!(!is_app_process("/usr/bin/java"));
        assert!(!is_app_process("/usr/bin/grok"));
    }
}
