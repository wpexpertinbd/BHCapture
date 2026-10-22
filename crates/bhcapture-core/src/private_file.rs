//! Writing captures safely: readable only by this user, never through a planted symlink.
//!
//! A screenshot or recording can hold anything that was on screen, and it may be saved into a folder
//! other accounts can write to (`/tmp`, `/Users/Shared`, a shared drive). Two things go wrong with a
//! plain `File::create` there:
//!
//! - it follows a symlink someone placed at the predicted name, so the capture lands in (or truncates)
//!   whatever file the link points at;
//! - under the usual 022 umask the new file is world-readable until a later `chmod`.
//!
//! So every capture is written to a NEW temporary file next to its destination -- created exclusively,
//! mode 0600 from the first byte -- and renamed into place. `rename` replaces a symlink at the
//! destination instead of following it, and a failed write never leaves a half file under the real
//! name.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// Whether anything at all is at `p` -- including a dangling symlink, which `Path::exists` reports as
/// absent. Use this to pick a free name.
pub fn occupied(p: &Path) -> bool { std::fs::symlink_metadata(p).is_ok() }

/// Create a file that did not exist before, readable and writable only by this user. Fails if
/// anything (a file, a directory, a symlink -- even a dangling one) is already at `path`.
pub fn create_new_private(path: &Path) -> io::Result<File> {
    let mut o = OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        // O_CREAT|O_EXCL (create_new) refuses ANY existing entry, a symlink included (POSIX), so no
        // O_NOFOLLOW is needed; the mode applies from creation, not after a chmod.
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
}

/// A temporary name next to `dest` that nothing else is using.
pub fn temp_beside(dest: &Path) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let n = N.fetch_add(1, Ordering::Relaxed);
    dest.with_file_name(format!(".{name}.{}-{n}.bhc-partial", std::process::id()))
}

/// Write `dest` through a private temporary file and an atomic rename (see the module notes).
/// `fill` writes the content; on any error the temporary file is removed and `dest` is untouched.
pub fn write_private_with(dest: &Path, fill: impl FnOnce(&mut File) -> io::Result<()>) -> io::Result<()> {
    let mut last = None;
    for _ in 0..8 {
        let tmp = temp_beside(dest);
        let mut f = match create_new_private(&tmp) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => { last = Some(e); continue; }
            Err(e) => return Err(e),
        };
        let done = fill(&mut f).and_then(|_| f.sync_all()).and_then(|_| { drop(f); std::fs::rename(&tmp, dest) });
        if done.is_err() { let _ = std::fs::remove_file(&tmp); }
        return done;
    }
    Err(last.unwrap_or_else(|| io::Error::new(io::ErrorKind::AlreadyExists, "no free temporary name")))
}

/// [`write_private_with`] for bytes already in memory.
pub fn write_private(dest: &Path, bytes: &[u8]) -> io::Result<()> {
    write_private_with(dest, |f| f.write_all(bytes))
}

/// Like [`write_private`], but `dest` must be NEW: if anything appears at that name before the write
/// lands (another save, another program), this fails with `AlreadyExists` instead of replacing it.
/// The final step is a hard link, which the OS refuses when the name is taken -- an atomic
/// "create only if absent" that a rename cannot give. A volume without hard links (FAT, some network
/// shares) falls back to checking the name just before renaming, the best those can do.
pub fn write_private_new(dest: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut last = None;
    for _ in 0..8 {
        let tmp = temp_beside(dest);
        let mut f = match create_new_private(&tmp) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => { last = Some(e); continue; }
            Err(e) => return Err(e),
        };
        let written = f.write_all(bytes).and_then(|_| f.sync_all());
        drop(f);
        let done = written.and_then(|_| match std::fs::hard_link(&tmp, dest) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(e),
            Err(_) if occupied(dest) => Err(io::Error::new(io::ErrorKind::AlreadyExists, "the name is taken")),
            Err(_) => std::fs::rename(&tmp, dest),
        });
        let _ = std::fs::remove_file(&tmp);
        return done;
    }
    Err(last.unwrap_or_else(|| io::Error::new(io::ErrorKind::AlreadyExists, "no free temporary name")))
}

/// Copy `src` to `dest` as a private file (see the module notes).
pub fn copy_private(src: &Path, dest: &Path) -> io::Result<()> {
    let mut from = File::open(src)?;
    write_private_with(dest, |f| io::copy(&mut from, f).map(|_| ()))
}

/// Move a finished file into place as a private file: a rename when both are on one volume, else a
/// private copy. `src` must be somewhere only this user can reach (it is made 0600 first).
pub fn move_private(src: &Path, dest: &Path) -> io::Result<()> {
    #[cfg(unix)]
    { use std::os::unix::fs::PermissionsExt; std::fs::set_permissions(src, std::fs::Permissions::from_mode(0o600))?; }
    match std::fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(_) => {
            copy_private(src, dest)?;
            let _ = std::fs::remove_file(src);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bhc-private-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn write_new_never_replaces_what_is_already_there() {
        let d = dir("new");
        let dest = d.join("shot edited.png");
        write_private_new(&dest, b"first").unwrap();
        let e = write_private_new(&dest, b"second").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&dest).unwrap(), b"first");
        // Nothing left behind but the one file.
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777, 0o600);
            // A dangling symlink at the name also counts as taken, and is not followed.
            let link = d.join("link.png");
            std::os::unix::fs::symlink(d.join("nowhere.png"), &link).unwrap();
            assert_eq!(write_private_new(&link, b"x").unwrap_err().kind(), io::ErrorKind::AlreadyExists);
            assert!(!d.join("nowhere.png").exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn new_files_are_0600_and_replace_a_planted_symlink_instead_of_following_it() {
        use std::os::unix::fs::PermissionsExt;
        let d = dir("link");
        let victim = d.join("victim.txt");
        std::fs::write(&victim, b"keep me").unwrap();
        let dest = d.join("shot.png");
        std::os::unix::fs::symlink(&victim, &dest).unwrap();
        write_private(&dest, b"pixels").unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep me", "the link was followed");
        assert!(!std::fs::symlink_metadata(&dest).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(&dest).unwrap(), b"pixels");
        assert_eq!(std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777, 0o600);
        // No temporary file is left behind.
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn create_new_refuses_a_dangling_symlink_and_occupied_sees_it() {
        let d = dir("dangling");
        let p = d.join("x");
        std::os::unix::fs::symlink(d.join("nowhere"), &p).unwrap();
        assert!(!p.exists(), "precondition: Path::exists misses a dangling link");
        assert!(occupied(&p));
        assert!(create_new_private(&p).is_err());
        assert!(!d.join("nowhere").exists());
    }

    #[test]
    fn a_failed_write_leaves_neither_the_destination_nor_a_temporary_file() {
        let d = dir("fail");
        let dest = d.join("out.mp4");
        let r = write_private_with(&dest, |_| Err(io::Error::new(io::ErrorKind::Other, "boom")));
        assert!(r.is_err());
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn copy_and_move_produce_private_files() {
        use std::os::unix::fs::PermissionsExt;
        let d = dir("copy");
        let src = d.join("src");
        std::fs::write(&src, b"abc").unwrap();
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o644)).unwrap();
        copy_private(&src, &d.join("c")).unwrap();
        assert_eq!(std::fs::metadata(d.join("c")).unwrap().permissions().mode() & 0o777, 0o600);
        move_private(&src, &d.join("m")).unwrap();
        assert!(!src.exists());
        assert_eq!(std::fs::metadata(d.join("m")).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
