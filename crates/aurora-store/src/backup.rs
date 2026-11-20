//! Backup taken before pending migrations: online copy to a temporary name,
//! verified, then renamed into place so only complete backups end in `.db`.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::backup::Backup;
use rusqlite::{Connection, OpenFlags};

use crate::{StoreError, integrity, platform};

const PAGES_PER_STEP: std::ffi::c_int = 256;
const STEP_PAUSE: Duration = Duration::ZERO;

/// Copies the open database into `backup_dir` and returns the final path.
/// `from_version` is the schema version recorded in the source.
pub(crate) fn take(
    source: &Connection,
    db_path: &Path,
    backup_dir: &Path,
    from_version: i64,
) -> Result<PathBuf, StoreError> {
    let fail = |path: &Path, reason: String| StoreError::BackupFailed {
        path: path.to_path_buf(),
        reason,
    };

    std::fs::create_dir_all(backup_dir).map_err(|e| fail(backup_dir, e.to_string()))?;
    remove_leftovers(backup_dir);

    let stem = db_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "aurora".to_owned());
    let stamp = utc_stamp(SystemTime::now());
    let base = format!("{stem}-v{from_version}-{stamp}");
    let mut final_path = backup_dir.join(format!("{base}.db"));
    let mut n = 1;
    while final_path.exists() {
        final_path = backup_dir.join(format!("{base}-{n}.db"));
        n += 1;
    }
    let mut tmp_name = final_path.as_os_str().to_os_string();
    tmp_name.push(".tmp");
    let tmp_path = PathBuf::from(tmp_name);

    let result = copy_verify_rename(source, &tmp_path, &final_path, from_version);
    if let Err(reason) = result {
        remove_tmp(&tmp_path);
        return Err(fail(&final_path, reason));
    }
    platform::sync_dir(backup_dir).map_err(|e| fail(backup_dir, e.to_string()))?;
    Ok(final_path)
}

fn copy_verify_rename(
    source: &Connection,
    tmp_path: &Path,
    final_path: &Path,
    from_version: i64,
) -> Result<(), String> {
    {
        let mut dest = Connection::open(tmp_path).map_err(|e| format!("cannot create: {e}"))?;
        Backup::new(source, &mut dest)
            .and_then(|b| b.run_to_completion(PAGES_PER_STEP, STEP_PAUSE, None))
            .map_err(|e| format!("copy failed: {e}"))?;
        // The copy inherits WAL mode from the source; a self-contained single
        // file is what a restore wants.
        dest.pragma_update(None, "journal_mode", "DELETE")
            .map_err(|e| format!("cannot switch the copy out of WAL: {e}"))?;
        dest.close()
            .map_err(|(_, e)| format!("cannot close the copy: {e}"))?;
    }
    std::fs::OpenOptions::new()
        .write(true)
        .open(tmp_path)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("cannot flush the copy: {e}"))?;

    let copy = Connection::open_with_flags(tmp_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("verification could not open the copy: {e}"))?;
    integrity::run(&copy, integrity::IntegrityCheck::Quick)
        .map_err(|d| format!("verification failed, the copy is damaged: {d}"))?;
    let copied: i64 = copy
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("verification could not read the copy's schema version: {e}"))?;
    if copied != from_version {
        return Err(format!(
            "verification failed: the copy is at schema version {copied}, expected {from_version}"
        ));
    }
    copy.close()
        .map_err(|(_, e)| format!("cannot close the verified copy: {e}"))?;

    std::fs::rename(tmp_path, final_path).map_err(|e| format!("cannot move into place: {e}"))
}

fn remove_tmp(tmp_path: &Path) {
    let _ = std::fs::remove_file(tmp_path);
    let mut journal = tmp_path.as_os_str().to_os_string();
    journal.push("-journal");
    let _ = std::fs::remove_file(journal);
}

/// Half-written copies from an earlier crash; best effort.
fn remove_leftovers(backup_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(backup_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".db.tmp") || name.ends_with(".db.tmp-journal") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// `YYYYMMDDTHHMMSSZ` in UTC.
fn utc_stamp(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_formats_known_instants() {
        assert_eq!(utc_stamp(UNIX_EPOCH), "19700101T000000Z");
        // 2024-02-29 12:34:56 UTC
        let t = UNIX_EPOCH + Duration::from_secs(1_709_210_096);
        assert_eq!(utc_stamp(t), "20240229T123456Z");
    }
}
