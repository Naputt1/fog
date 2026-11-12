//! `fog prune`: remove the captured logs of finished instances.
//!
//! A running instance's logs are never touched, so a prune can never blind a
//! live `fog logs`.

use crate::cli::presentation::format_bytes;
use crate::ipc;
use std::io;

/// Removes the captured logs of instances that have finished.
///
/// A running instance's logs are never touched. `--dry-run` reports what would
/// be removed without deleting anything; `--verbose` lists each directory.
pub(crate) fn cmd_prune(dry_run: bool, verbose: bool) -> io::Result<()> {
    let summary = ipc::prune_instance_dirs(&ipc::instance_dir(), false, dry_run);
    if summary.removed.is_empty() {
        println!("nothing to prune: no finished instances");
    } else {
        println!(
            "{} logs of {} finished instance{} ({})",
            if dry_run { "would prune" } else { "pruned" },
            summary.removed.len(),
            if summary.removed.len() == 1 { "" } else { "s" },
            format_bytes(summary.bytes),
        );
        if verbose {
            for path in &summary.removed {
                println!("  {}", path.display());
            }
        }
    }
    if summary.sockets > 0 {
        println!(
            "{} {} stale socket{}",
            if dry_run {
                "would remove"
            } else {
                "also removed"
            },
            summary.sockets,
            if summary.sockets == 1 { "" } else { "s" },
        );
    }
    if summary.running > 0 {
        println!(
            "kept {} running instance{}",
            summary.running,
            if summary.running == 1 { "" } else { "s" }
        );
    }
    Ok(())
}
