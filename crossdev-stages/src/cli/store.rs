use std::collections::BTreeSet;

use camino::{Utf8Path, Utf8PathBuf};

use crate::cli::StoreCmd;
use crate::error::Result;
use crate::workspace::Workspace;

pub fn run(ws: &Workspace, boards_root: &Utf8Path, cmd: StoreCmd) -> Result<()> {
    match cmd {
        StoreCmd::List => list(ws),
        StoreCmd::Gc { force } => gc(ws, boards_root, force),
    }
}

fn list(ws: &Workspace) -> Result<()> {
    let entries = walk_store(ws);
    if entries.is_empty() {
        println!("Store is empty.");
        return Ok(());
    }
    println!("{:<32} {:<14} {}", "chost", "hash", "state");
    for e in &entries {
        let state = if e.complete { "complete" } else { "partial" };
        println!("{:<32} {:<14} {state}", e.chost, e.hash);
    }
    println!("\n{} entries.", entries.len());
    Ok(())
}

fn gc(ws: &Workspace, boards_root: &Utf8Path, force: bool) -> Result<()> {
    let entries = walk_store(ws);
    let binpkgs_entries = walk_binpkgs(ws);
    let live = live_set(boards_root)?;

    let mut unused: Vec<StoreEntry> = entries
        .into_iter()
        .filter(|e| !live.contains(&(e.chost.clone(), e.hash.clone())))
        .collect();
    let mut unused_binpkgs: Vec<StoreEntry> = binpkgs_entries
        .into_iter()
        .filter(|e| !live.contains(&(e.chost.clone(), e.hash.clone())))
        .collect();

    if unused.is_empty() && unused_binpkgs.is_empty() {
        println!("No unused store or binpkg entries.");
        return Ok(());
    }

    if !unused.is_empty() {
        println!("{} unused store entr{}:", unused.len(), if unused.len() == 1 { "y" } else { "ies" });
        for e in &unused {
            let state = if e.complete { "complete" } else { "partial" };
            let size = dir_size_human(&e.path);
            println!("  {:<32} {:<14} {state:<8} {size}", e.chost, e.hash);
        }
    }
    if !unused_binpkgs.is_empty() {
        println!(
            "{} unused binpkg cache{}:",
            unused_binpkgs.len(),
            if unused_binpkgs.len() == 1 { "" } else { "s" },
        );
        for e in &unused_binpkgs {
            let size = dir_size_human(&e.path);
            println!("  {:<32} {:<14} {size}", e.chost, e.hash);
        }
    }
    if !force {
        println!("\nRe-run with --force to delete.");
        return Ok(());
    }

    let mut removed = 0;
    for e in unused.drain(..).chain(unused_binpkgs.drain(..)) {
        match crate::container::destroy_dir(&e.path, ws.base()) {
            Ok(()) => {
                println!("Removed {}/{}", e.chost, e.hash);
                removed += 1;
            }
            Err(err) => println!("Failed to remove {}/{}: {err}", e.chost, e.hash),
        }
    }
    println!("\nRemoved {removed} entr{}.", if removed == 1 { "y" } else { "ies" });
    Ok(())
}

struct StoreEntry {
    chost: String,
    hash: String,
    complete: bool,
    path: Utf8PathBuf,
}

fn walk_store(ws: &Workspace) -> Vec<StoreEntry> {
    walk_two_level(&ws.store_dir(), |path| path.join(".complete").exists())
}

fn walk_binpkgs(ws: &Workspace) -> Vec<StoreEntry> {
    walk_two_level(&ws.binpkgs_dir(), |_| false)
}

fn walk_two_level(
    root: &Utf8Path,
    mark_complete: impl Fn(&Utf8Path) -> bool,
) -> Vec<StoreEntry> {
    let Ok(chost_iter) = std::fs::read_dir(root) else { return vec![] };
    let mut out = Vec::new();
    for chost_entry in chost_iter.flatten() {
        let Some(chost) = chost_entry.file_name().to_str().map(String::from) else { continue };
        let chost_path = match Utf8PathBuf::try_from(chost_entry.path()) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let Ok(hash_iter) = std::fs::read_dir(&chost_path) else { continue };
        for hash_entry in hash_iter.flatten() {
            let Some(hash) = hash_entry.file_name().to_str().map(String::from) else { continue };
            let path = match Utf8PathBuf::try_from(hash_entry.path()) {
                Ok(p) => p,
                Err(_) => continue,
            };
            let complete = mark_complete(&path);
            out.push(StoreEntry { chost: chost.clone(), hash, complete, path });
        }
    }
    out.sort_by(|a, b| (a.chost.as_str(), a.hash.as_str()).cmp(&(b.chost.as_str(), b.hash.as_str())));
    out
}

/// (chost, hash) pairs every known board would resolve to.  A store
/// entry NOT in this set is unreachable from the project's boards
/// and a candidate for GC.
fn live_set(boards_root: &Utf8Path) -> Result<BTreeSet<(String, String)>> {
    let mut set = BTreeSet::new();
    for name in crate::board::list(boards_root)? {
        let Ok(b) = crate::board::load(boards_root, &name) else { continue };
        let chost = format!("{}-unknown-linux-gnu", b.arch);
        let (_, hash) = crate::cflags::canonicalize(&b.effective_cflags());
        set.insert((chost, hash));
    }
    // Default-cflags hash is what `target stage1 / update / install`
    // resolve to without a board context.  Keep them too.
    let arches: BTreeSet<String> = crate::board::list(boards_root)?
        .iter()
        .filter_map(|n| crate::board::load(boards_root, n).ok())
        .map(|b| b.arch.clone())
        .collect();
    for arch in arches {
        let chost = format!("{arch}-unknown-linux-gnu");
        let (_, hash) = crate::cflags::canonicalize(crate::stage::default_cflags(&arch));
        set.insert((chost, hash));
    }
    Ok(set)
}

fn dir_size_human(p: &Utf8Path) -> String {
    let bytes = walk_size(p);
    if bytes >= 1_073_741_824 {
        format!("{:.1}G", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1}M", bytes as f64 / 1_048_576.0)
    } else {
        format!("{}K", bytes / 1024)
    }
}

fn walk_size(p: &Utf8Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(p) else { return 0 };
    if meta.is_file() {
        return meta.len();
    }
    if !meta.is_dir() {
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(p) else { return 0 };
    let mut total = 0;
    for e in entries.flatten() {
        if let Ok(child) = Utf8PathBuf::try_from(e.path()) {
            total += walk_size(&child);
        }
    }
    total
}
