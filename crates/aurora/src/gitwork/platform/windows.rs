//! Windows: no console window for git, `\\?\` prefixes dropped from canonical
//! paths, git's forward slashes turned into backslashes, case-insensitive lock
//! keys, long-path support switched on for git.

use std::path::{Path, PathBuf};

/// `CREATE_NO_WINDOW`: git never gets a console window of its own.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn no_window(command: &mut tokio::process::Command) {
    command.creation_flags(CREATE_NO_WINDOW);
}

/// Git for Windows stops at `MAX_PATH` unless told otherwise.
pub fn config_args() -> &'static [&'static str] {
    &["-c", "core.longpaths=true"]
}

/// `\\?\C:\x` becomes `C:\x` and `\\?\UNC\server\share` becomes
/// `\\server\share`: git and most tools do not take the verbatim form.
pub fn plain(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        let bytes = rest.as_bytes();
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            return PathBuf::from(rest);
        }
    }
    path.to_path_buf()
}

/// The path of an existing folder, canonical and without the verbatim prefix.
pub fn canonical(path: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(path).map(|found| plain(&found))
}

/// A path as git prints it (`C:/Users/me/repo`, or `/c/Users/me/repo` from an
/// msys build) as a Windows path.
pub fn from_git_output(text: &str) -> PathBuf {
    let text = text.trim();
    let bytes = text.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b'/' {
        let drive = char::from(bytes[1]).to_ascii_uppercase();
        return PathBuf::from(format!("{drive}:{}", text[2..].replace('/', "\\")));
    }
    PathBuf::from(text.replace('/', "\\"))
}

/// Paths that differ only in case or in the kind of slash are one repository.
pub fn lock_key(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbatim_prefixes_are_dropped() {
        assert_eq!(
            plain(Path::new(r"\\?\C:\Users\me\repo")),
            Path::new(r"C:\Users\me\repo")
        );
        assert_eq!(
            plain(Path::new(r"\\?\UNC\server\share\repo")),
            Path::new(r"\\server\share\repo")
        );
        assert_eq!(plain(Path::new(r"C:\plain")), Path::new(r"C:\plain"));
        // Not a drive: left as it is rather than guessed at.
        assert_eq!(
            plain(Path::new(r"\\?\Volume{1}\x")),
            Path::new(r"\\?\Volume{1}\x")
        );
    }

    #[test]
    fn git_output_becomes_a_windows_path() {
        assert_eq!(
            from_git_output("C:/Users/me/repo\n"),
            Path::new(r"C:\Users\me\repo")
        );
        assert_eq!(
            from_git_output("/c/Users/me/repo"),
            Path::new(r"C:\Users\me\repo")
        );
        assert_eq!(from_git_output("D:/"), Path::new(r"D:\"));
        assert_eq!(
            from_git_output("//server/share/r"),
            Path::new(r"\\server\share\r")
        );
    }

    #[test]
    fn lock_keys_ignore_case_and_slash_kind() {
        assert_eq!(
            lock_key(Path::new("C:/Users/Me/Repo")),
            lock_key(Path::new(r"c:\users\me\repo"))
        );
    }

    #[test]
    fn canonical_has_no_verbatim_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let found = canonical(dir.path()).unwrap();
        assert!(!found.to_string_lossy().starts_with(r"\\?\"), "{found:?}");
        assert!(found.is_dir());
    }
}
