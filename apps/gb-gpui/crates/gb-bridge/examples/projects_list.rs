//! List-only cleanup of GitButler's project list. Removes entries from
//! `<app-data>/projects.json` after a verified backup. Never deletes a file,
//! a directory or a ref, and refuses while the GitButler app runs.
//!
//! cargo run -p gb-bridge --example projects_list -- --app-data <dir> [--keep <path>]... [--dry-run]
//! cargo run -p gb-bridge --example projects_list -- --app-data <dir> --restore <backup-file>

use std::path::PathBuf;

use gb_bridge::projects_list::{
    PsProbe, Selector, default_backup_dir, remove_projects_from_list, restore_projects_from_backup,
};

fn main() {
    if let Err(message) = run() {
        eprintln!("error: {message}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut app_data: Option<PathBuf> = None;
    let mut backup_dir = default_backup_dir();
    let mut keep: Vec<String> = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    let mut restore: Option<PathBuf> = None;
    let mut dry_run = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--app-data" => app_data = Some(PathBuf::from(value("--app-data")?)),
            "--backup-dir" => backup_dir = PathBuf::from(value("--backup-dir")?),
            "--keep" => keep.push(value("--keep")?),
            "--id" => ids.push(value("--id")?),
            "--restore" => restore = Some(PathBuf::from(value("--restore")?)),
            "--dry-run" => dry_run = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let app_data = app_data.ok_or("--app-data <dir> is required (no implicit default)")?;
    if let Some(file) = restore {
        let previous = restore_projects_from_backup(&app_data, &file, &backup_dir, &PsProbe)
            .map_err(|e| e.message)?;
        println!("restored {}", file.display());
        println!("previous list saved at {}", previous.display());
        return Ok(());
    }
    let selector = if ids.is_empty() {
        Selector::AllExceptPaths(&keep)
    } else {
        Selector::Ids(&ids)
    };
    let report = remove_projects_from_list(&app_data, selector, &backup_dir, &PsProbe, dry_run)
        .map_err(|e| e.message)?;
    println!("{}", if report.dry_run { "dry run" } else { "applied" });
    for entry in &report.removed {
        println!("remove  {}  {}  {}", entry.id, entry.title, entry.path);
    }
    for entry in &report.kept {
        println!("keep    {}  {}  {}", entry.id, entry.title, entry.path);
    }
    if let Some(path) = &report.backup_path {
        println!("backup  {}", path.display());
    }
    Ok(())
}
