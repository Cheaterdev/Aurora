//! Refuses database locations that cannot give SQLite the locking and
//! flushing guarantees it needs: network shares and cloud-synced folders.

use std::path::Path;

use crate::platform;

/// Folder-name prefixes (lowercase) of sync clients. A heuristic: it misses
/// renamed folders and can flag harmless ones, which is why it can be overridden.
const SYNCED_FOLDER_PREFIXES: &[&str] = &[
    "onedrive",
    "dropbox",
    "google drive",
    "googledrive",
    "icloud drive",
    "iclouddrive",
];

/// The reason `path` is unsafe for a database, or `None` when it looks fine.
pub fn unsafe_reason(path: &Path) -> Option<String> {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    platform::network_location(&absolute).or_else(|| synced_folder_reason(&absolute))
}

pub fn synced_folder_reason(path: &Path) -> Option<String> {
    path.components().find_map(|component| {
        let name = component.as_os_str().to_string_lossy();
        let lower = name.to_lowercase();
        SYNCED_FOLDER_PREFIXES
            .iter()
            .any(|prefix| lower.starts_with(prefix))
            .then(|| format!("it is inside {name:?}, which looks like a cloud-synced folder"))
    })
}
