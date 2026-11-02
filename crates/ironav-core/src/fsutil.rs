//! File replacement that readers never see half done.

use std::io;
use std::path::Path;
use std::time::Duration;

/// Moves `from` over `to`, replacing it in one step, so a reader sees the old file or the new one
/// and never a mix. On Windows the move fails while another process has `to` open without
/// allowing deletion (a player reading a playlist, an indexer, a virus scanner); that is retried
/// for up to about a second before the error is returned.
pub fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    let mut wait = Duration::from_millis(5);
    for _ in 0..12 {
        match std::fs::rename(from, to) {
            Err(e) if cfg!(windows) && is_sharing(&e) => {
                std::thread::sleep(wait);
                wait = (wait * 2).min(Duration::from_millis(200));
            }
            r => return r,
        }
    }
    std::fs::rename(from, to)
}

/// Creates `path` for writing and fails if anything is there already, a file or a symbolic link
/// (`O_EXCL`, `CREATE_NEW`), so a temporary output name cannot be turned into a way to write
/// through a link someone else put there. A name left behind by a crashed run is refused with
/// a hint to remove it.
pub fn create_new(path: &Path) -> crate::Result<std::fs::File> {
    std::fs::OpenOptions::new().write(true).create_new(true).open(path).map_err(|e| {
        if e.kind() == io::ErrorKind::AlreadyExists {
            crate::Error::new(crate::ErrorCode::OutputExists, format!("'{}' exists already", path.display()))
                .hint("it is the temporary name of an output and was left by an earlier run that did not finish (or put there by someone else); remove it and try again")
        } else {
            e.into()
        }
    })
}

/// ERROR_ACCESS_DENIED (5), ERROR_SHARING_VIOLATION (32) and ERROR_LOCK_VIOLATION (33).
fn is_sharing(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(5 | 32 | 33)) || e.kind() == io::ErrorKind::PermissionDenied
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_replaced_whole() {
        let d = std::env::temp_dir().join(format!("ironav-replace-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let (a, b) = (d.join("a.partial"), d.join("a"));
        std::fs::write(&b, b"old").unwrap();
        std::fs::write(&a, b"new").unwrap();
        replace_file(&a, &b).unwrap();
        assert_eq!(std::fs::read(&b).unwrap(), b"new");
        assert!(!a.exists());
        // a missing source is an error at once, not after retries
        assert!(replace_file(&a, &b).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn create_new_refuses_anything_already_there() {
        let d = std::env::temp_dir().join(format!("ironav-createnew-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("x.partial");
        create_new(&f).unwrap();
        let e = create_new(&f).unwrap_err();
        assert_eq!(e.code, crate::ErrorCode::OutputExists);
        assert!(e.hint.unwrap().contains("remove it"));
        // a link there (to a file that exists) is not followed or truncated
        let victim = d.join("victim");
        std::fs::write(&victim, b"keep").unwrap();
        let link = d.join("link.partial");
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&victim, &link).is_ok();
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(&victim, &link).is_ok();
        if made {
            assert_eq!(create_new(&link).unwrap_err().code, crate::ErrorCode::OutputExists);
            assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(windows)]
    #[test]
    fn a_reader_holding_the_file_briefly_does_not_fail_the_replace() {
        use std::os::windows::fs::OpenOptionsExt;
        let d = std::env::temp_dir().join(format!("ironav-replace-held-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let (a, b) = (d.join("p.partial"), d.join("p"));
        std::fs::write(&b, b"old").unwrap();
        std::fs::write(&a, b"new").unwrap();
        // opened without FILE_SHARE_DELETE, as many readers do, and let go after 50 ms
        let held = std::fs::OpenOptions::new().read(true).share_mode(1).open(&b).unwrap();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            drop(held);
        });
        replace_file(&a, &b).unwrap();
        t.join().unwrap();
        assert_eq!(std::fs::read(&b).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(&d);
    }
}
