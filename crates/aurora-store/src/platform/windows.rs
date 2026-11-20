//! Windows specifics: network drives and UNC shares.

use std::path::{Component, Path, Prefix};

use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;

// windows-sys only exposes this under another feature; the value is fixed by the Win32 API.
const DRIVE_REMOTE: u32 = 4;

/// Windows cannot flush a directory handle through std; NTFS journals the
/// rename itself.
pub fn sync_dir(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Why `path` (already absolute) is on a network location, if it is.
pub fn network_location(path: &Path) -> Option<String> {
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return None;
    };
    match prefix.kind() {
        Prefix::UNC(..) | Prefix::VerbatimUNC(..) => {
            Some("it is on a network share (UNC path)".to_owned())
        }
        Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
            let root: Vec<u16> = [letter as u16, b':' as u16, b'\\' as u16, 0].to_vec();
            // SAFETY: `root` is a NUL-terminated UTF-16 string that outlives the call.
            let kind = unsafe { GetDriveTypeW(root.as_ptr()) };
            (kind == DRIVE_REMOTE)
                .then(|| format!("drive {}: is a network drive", char::from(letter)))
        }
        _ => None,
    }
}
