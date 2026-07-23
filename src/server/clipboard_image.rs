use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const STAGED_CLIPBOARD_IMAGE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

pub(crate) struct StagedClipboardImage {
    pub(crate) path: PathBuf,
    pub(crate) paste_text: String,
}

pub(crate) fn stage(
    client_id: u64,
    extension: &str,
    data: &[u8],
) -> io::Result<StagedClipboardImage> {
    let extension = sanitize_extension(extension);
    let dir = ensure_staging_dir()?;
    cleanup_stale(&dir);

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);

    for attempt in 0..100 {
        let path = dir.join(format!(
            "client-{client_id}-clipboard-{unique}-{attempt}.{extension}"
        ));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        restrict_file_options(&mut options);
        let mut file = match options.open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        };
        file.write_all(data)?;
        return Ok(StagedClipboardImage {
            paste_text: path.to_string_lossy().into_owned(),
            path,
        });
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "failed to allocate unique clipboard image staging path",
    ))
}

pub(crate) fn remove_files(paths: Vec<PathBuf>) {
    for path in paths {
        let _ = fs::remove_file(path);
    }
}

fn sanitize_extension(extension: &str) -> &'static str {
    if extension.eq_ignore_ascii_case("png") {
        "png"
    } else if extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg") {
        "jpg"
    } else if extension.eq_ignore_ascii_case("gif") {
        "gif"
    } else if extension.eq_ignore_ascii_case("webp") {
        "webp"
    } else if extension.eq_ignore_ascii_case("bmp") {
        "bmp"
    } else {
        "png"
    }
}

/// Also used by `remote::federation::file_staging`, which validates this path
/// before letting anything create it.
pub(crate) fn staging_dir() -> PathBuf {
    #[cfg(unix)]
    // SAFETY: `geteuid` only reads a property of the calling process. It takes
    // no arguments, touches no caller memory, and is documented as always
    // succeeding.
    let user_id = unsafe { libc::geteuid() };
    #[cfg(windows)]
    let user_id = std::process::id();
    std::env::temp_dir().join(format!("herdr-clipboard-images-{user_id}"))
}

/// Also used by `remote::federation::file_staging`: the federated paste path
/// deliberately shares this one euid-scoped directory, its permissions, and its
/// 24h sweep rather than standing up a second staging root.
pub(crate) fn ensure_staging_dir() -> io::Result<PathBuf> {
    let dir = staging_dir();
    ensure_staging_dir_at(&dir)?;
    Ok(dir)
}

/// Proves `dir` is a private directory this user owns, creating it if absent,
/// before any caller chmods, sweeps, or writes inside it.
///
/// The staging root lives at a predictable name under a possibly shared
/// `TMPDIR`, so on a multi-user host another user can pre-create that name as a
/// symlink to a directory they want destroyed. `create_dir_all`, `metadata`,
/// and `set_permissions` all follow symlinks and would happily adopt, chmod,
/// and later sweep the target — so the entry is inspected with
/// `symlink_metadata` and an unowned or non-directory root is refused outright
/// rather than repaired.
#[cfg(unix)]
pub(crate) fn ensure_staging_dir_at(dir: &Path) -> io::Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(metadata) => verify_private_owned_directory(dir, &metadata)?,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            create_private_dir(dir)?;
            // Creation alone proves nothing about what is now at the path:
            // `create_dir_all` also succeeds when the name already resolved to
            // a directory through a symlink planted since the check above.
            let metadata = fs::symlink_metadata(dir)?;
            verify_private_owned_directory(dir, &metadata)?;
        }
        Err(err) => return Err(err),
    }
    restrict_dir_permissions(dir)
}

#[cfg(windows)]
pub(crate) fn ensure_staging_dir_at(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let metadata = fs::metadata(dir)?;
    if !metadata.is_dir() {
        return Err(io::Error::other(format!(
            "clipboard image staging path is not a directory: {}",
            dir.display()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn verify_private_owned_directory(dir: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    use std::os::unix::fs::PermissionsExt as _;

    // SAFETY: see `staging_dir`.
    let euid = unsafe { libc::geteuid() };
    match staging_root_defect(
        metadata.file_type().is_symlink(),
        metadata.is_dir(),
        metadata.uid(),
        metadata.permissions().mode(),
        euid,
    ) {
        None => Ok(()),
        Some(defect) => Err(io::Error::other(format!(
            "clipboard image staging {defect}: {}",
            dir.display()
        ))),
    }
}

/// The decision itself, split out from the `stat` so the refusal for a root
/// owned by somebody else is reachable in a test without being that user.
#[cfg(unix)]
fn staging_root_defect(
    is_symlink: bool,
    is_dir: bool,
    uid: u32,
    mode: u32,
    euid: u32,
) -> Option<&'static str> {
    if is_symlink || !is_dir {
        return Some("path is not a directory");
    }
    if uid != euid {
        return Some("directory is owned by another user");
    }
    // A root that is or ever was group- or world-accessible may already hold
    // entries planted by another user, and the 24h sweep deletes whatever it
    // finds. Refuse it instead of tightening it after the fact.
    if mode & 0o077 != 0 {
        return Some("directory is accessible to other users");
    }
    None
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// Also used by `remote::federation::file_staging` so the federated paste path
/// gets the identical 0600 creation mode instead of re-deriving it.
#[cfg(unix)]
pub(crate) fn restrict_file_options(options: &mut fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;

    options.mode(0o600);
}

#[cfg(windows)]
pub(crate) fn restrict_file_options(_options: &mut fs::OpenOptions) {}

#[cfg(unix)]
fn restrict_dir_permissions(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

/// Also used by `remote::federation::file_staging`, which runs this same 24h
/// sweep before its quota scan so the quota is measured after eviction.
pub(crate) fn cleanup_stale(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if modified.elapsed().unwrap_or_default() > STAGED_CLIPBOARD_IMAGE_MAX_AGE {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_extension_accepts_known_image_extensions() {
        assert_eq!(sanitize_extension("PNG"), "png");
        assert_eq!(sanitize_extension("jpeg"), "jpg");
        assert_eq!(sanitize_extension("webp"), "webp");
        assert_eq!(sanitize_extension("sh"), "png");
    }

    #[cfg(unix)]
    mod staging_root {
        use super::*;
        use std::os::unix::fs::PermissionsExt as _;

        /// A private scratch directory. Never the real staging root: that one is
        /// process-global and sibling tests in this binary run as threads.
        struct Scratch {
            path: PathBuf,
        }

        impl Scratch {
            fn new(label: &str) -> Self {
                let unique = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_nanos())
                    .unwrap_or(0);
                let path = std::env::temp_dir().join(format!(
                    "herdr-clipboard-root-test-{}-{label}-{unique}",
                    std::process::id()
                ));
                fs::create_dir_all(&path).expect("create scratch dir");
                Self { path }
            }
        }

        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.path);
            }
        }

        fn mode_of(path: &Path) -> u32 {
            fs::metadata(path).expect("metadata").permissions().mode() & 0o777
        }

        #[test]
        fn a_staging_root_owned_by_another_user_is_refused() {
            const DIR_MODE: u32 = 0o040700;
            let euid = 1000;

            assert_eq!(
                staging_root_defect(false, true, euid, DIR_MODE, euid),
                None,
                "a private directory this user owns must be accepted"
            );
            assert_eq!(
                staging_root_defect(false, true, euid + 1, DIR_MODE, euid),
                Some("directory is owned by another user"),
                "a directory owned by another user must be refused"
            );
            assert_eq!(
                staging_root_defect(true, false, euid, DIR_MODE, euid),
                Some("path is not a directory"),
                "a symlink must be refused even when this user owns the link"
            );
            assert_eq!(
                staging_root_defect(false, true, euid, 0o040707, euid),
                Some("directory is accessible to other users"),
                "a directory other users can reach must be refused"
            );
        }

        #[test]
        fn ensure_staging_dir_at_creates_a_private_directory() {
            let scratch = Scratch::new("create");
            let root = scratch.path.join("root");
            ensure_staging_dir_at(&root).expect("a fresh root is created");
            assert_eq!(mode_of(&root), 0o700, "fresh staging root is not private");
            // Idempotent: a second call accepts the root it just made.
            ensure_staging_dir_at(&root).expect("an owned private root is accepted");
        }

        #[test]
        fn ensure_staging_dir_at_refuses_a_symlinked_root_without_touching_the_target() {
            let scratch = Scratch::new("symlink");
            let victim = scratch.path.join("victim");
            fs::create_dir_all(&victim).expect("create victim dir");
            fs::write(victim.join("id_rsa"), b"secret").expect("write victim file");
            fs::set_permissions(&victim, fs::Permissions::from_mode(0o755))
                .expect("set victim mode");

            let root = scratch.path.join("root");
            std::os::unix::fs::symlink(&victim, &root).expect("plant symlink");

            let err =
                ensure_staging_dir_at(&root).expect_err("a symlinked staging root must be refused");
            assert!(
                err.to_string().contains("not a directory"),
                "unexpected error: {err}"
            );
            assert_eq!(
                mode_of(&victim),
                0o755,
                "the symlink target was chmod'd through the link"
            );
            assert!(
                victim.join("id_rsa").exists(),
                "the symlink target's contents were exposed to the sweep"
            );
        }

        #[test]
        fn ensure_staging_dir_at_refuses_a_root_reachable_by_other_users() {
            let scratch = Scratch::new("loose");
            let root = scratch.path.join("root");
            fs::create_dir_all(&root).expect("create root");
            fs::set_permissions(&root, fs::Permissions::from_mode(0o777)).expect("loosen root");

            let err = ensure_staging_dir_at(&root)
                .expect_err("a world-writable staging root must be refused");
            assert!(
                err.to_string().contains("accessible to other users"),
                "unexpected error: {err}"
            );
            assert_eq!(
                mode_of(&root),
                0o777,
                "a refused root was repaired instead of rejected"
            );
        }

        #[test]
        fn ensure_staging_dir_at_refuses_a_root_that_is_a_regular_file() {
            let scratch = Scratch::new("file");
            let root = scratch.path.join("root");
            fs::write(&root, b"not a directory").expect("create file");

            let err =
                ensure_staging_dir_at(&root).expect_err("a file staging root must be refused");
            assert!(
                err.to_string().contains("not a directory"),
                "unexpected error: {err}"
            );
            assert_eq!(
                fs::read(&root).expect("root still readable"),
                b"not a directory"
            );
        }
    }
}
