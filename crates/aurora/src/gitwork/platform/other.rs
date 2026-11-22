//! Unix: nothing to hide, nothing to translate.

use std::path::{Path, PathBuf};

pub fn no_window(_command: &mut tokio::process::Command) {}

pub fn config_args() -> &'static [&'static str] {
    &[]
}

pub fn plain(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// The path of an existing folder, canonical.
pub fn canonical(path: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(path)
}

pub fn from_git_output(text: &str) -> PathBuf {
    PathBuf::from(text.trim())
}

pub fn lock_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_output_is_taken_as_it_is() {
        assert_eq!(
            from_git_output("/home/me/repo\n"),
            Path::new("/home/me/repo")
        );
    }
}
