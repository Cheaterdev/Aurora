//! Platforms without a network-location check yet; the synced-folder name
//! heuristic still applies.

use std::path::Path;

pub fn network_location(_path: &Path) -> Option<String> {
    None
}
