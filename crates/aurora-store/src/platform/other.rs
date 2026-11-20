//! Platforms without a network-location check yet; the synced-folder name
//! heuristic still applies.

use std::path::Path;

/// Makes a rename inside `dir` durable.
pub fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

pub fn network_location(_path: &Path) -> Option<String> {
    None
}
