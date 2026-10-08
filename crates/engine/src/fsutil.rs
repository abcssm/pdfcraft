//! Writing files safely: every output the app or an agent produces goes through
//! [`write_atomic`], so a crash or full disk never leaves a half-written file where the
//! original was, and a planted link can't redirect the write.

use std::hash::{BuildHasher, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// How many unpredictable temporary names to try before giving up.
const ATTEMPTS: u32 = 16;

/// Write `bytes` to `path` atomically: stage them in a new temporary file beside it, flush them
/// to disk, then rename over the target.
///
/// The temporary file has an unpredictable name and is created exclusively (`create_new`), so a
/// file or link already sitting at that name — say, one planted inside an untrusted folder to
/// point outside it — is never followed or overwritten. Replacing an existing file keeps its
/// Unix permissions, so saving over a private (0600) PDF doesn't make it world-readable.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = path.file_name().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a file path"))?;
    let (tmp, mut file) = create_temp(dir, &name.to_string_lossy())?;
    let result = (|| {
        file.write_all(bytes)?;
        #[cfg(unix)]
        if let Ok(meta) = std::fs::metadata(path) {
            file.set_permissions(meta.permissions())?;
        }
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// A new, exclusively created file `.{name}.{random}.pdfcraft-tmp` in `dir`.
fn create_temp(dir: &Path, name: &str) -> std::io::Result<(PathBuf, std::fs::File)> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut last = None;
    for _ in 0..ATTEMPTS {
        // `RandomState` is seeded from the OS's random source; the counter keeps names distinct
        // within this process even where that source is weak.
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        let tmp = dir.join(format!(".{name}.{:016x}.pdfcraft-tmp", h.finish()));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(f) => return Ok((tmp, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("could not create a temporary file")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pdfcraft-fsutil-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn writes_replace_the_target_and_leave_no_temporary_files() {
        let d = scratch("replace");
        let target = d.join("out.pdf");
        write_atomic(&target, b"one").unwrap();
        write_atomic(&target, b"two").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"two");
        let names: Vec<_> = std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, ["out.pdf"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_file_at_the_old_predictable_temporary_name_is_left_alone() {
        // The temporary name used to be fixed (`.out.pdf.pdfcraft-tmp`) and written with
        // `fs::write`, which follows a link planted there to a file outside the folder.
        let d = scratch("planted");
        let planted = d.join(".out.pdf.pdfcraft-tmp");
        std::fs::write(&planted, "SENTINEL").unwrap();
        write_atomic(&d.join("out.pdf"), b"data").unwrap();
        assert_eq!(std::fs::read_to_string(&planted).unwrap(), "SENTINEL");
        assert_eq!(std::fs::read(d.join("out.pdf")).unwrap(), b"data");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn a_planted_symlink_is_never_written_through() {
        let d = scratch("symlink");
        let outside = d.join("outside.txt");
        std::fs::write(&outside, "SENTINEL").unwrap();
        // Even if the random name were guessed, `create_new` refuses an existing link.
        let link = d.join("guessed");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let e = std::fs::OpenOptions::new().write(true).create_new(true).open(&link).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "SENTINEL");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn replacing_a_private_file_keeps_it_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = scratch("perms");
        let target = d.join("secret.pdf");
        std::fs::write(&target, "old").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        write_atomic(&target, b"new").unwrap();
        assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&d);
    }
}
