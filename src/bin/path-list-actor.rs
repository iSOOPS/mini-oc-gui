//! `path-list-actor` CLI — manage `path-list.md` entries from the terminal.
//!
//! Mirrors `path-list-actor.py`:
//! - `add <path>`    — validate, upsert, print table
//! - `list`          — print current entries
//! - `remove <path>` — validate, remove, print table

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use mini_oc_gui_serve::{
    domain::{PathEntry, PathValidator},
    error::AppError,
    storage::{PathListStore, cache::FileCache},
};

#[derive(Parser)]
#[command(name = "path-list-actor", about = "Manage the path-list.md index")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Add or merge a path entry.
    Add {
        /// Absolute path, or ~/foo, ./foo, ../foo
        path: String,
    },
    /// List all entries.
    List,
    /// Remove a path entry.
    Remove {
        /// Path to remove.
        path: String,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), AppError> {
    let cli = Cli::parse();
    let cache = FileCache::new(PathBuf::from("path-list.md"));
    let store = PathListStore::new(cache);

    match cli.cmd {
        Cmd::Add { path } => {
            let normalized = PathValidator::validate(&path)?;
            let entries = store.upsert_path(&normalized).await?;
            print_table(&entries);
        }
        Cmd::List => {
            let entries = store.list().await?;
            print_table(&entries);
        }
        Cmd::Remove { path } => {
            let normalized = PathValidator::validate(&path)?;
            let entries = store.remove_path(&normalized).await?;
            print_table(&entries);
        }
    }
    Ok(())
}

fn print_table(entries: &[PathEntry]) {
    if entries.is_empty() {
        println!("(empty)");
        return;
    }
    let mut rows: Vec<(&str, usize, String)> = entries
        .iter()
        .map(|e| {
            let first3 = e
                .sections
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            let first3 = if first3.is_empty() {
                "(empty)".to_string()
            } else {
                first3
            };
            (e.path.as_str(), e.sections.len(), first3)
        })
        .collect();

    let w_path = rows.iter().map(|r| r.0.len()).max().unwrap_or(4).max(4);
    let w_secs = rows.iter().map(|r| r.1.to_string().len()).max().unwrap_or(8).max(8);

    println!("{:<w_path$}  {:>w_secs$}  first 3 section ids", "path", "sections");
    println!("{:<w_path$}  {:>w_secs$}  {}", "-".repeat(w_path), "-".repeat(w_secs), "-".repeat(20));
    rows.sort_by(|a, b| a.0.cmp(b.0));
    for (path, secs, ids) in rows {
        println!("{path:<w_path$}  {secs:>w_secs$}  {ids}");
    }
}
