#![cfg(unix)]
//! Pure recognition of "a pasted string is actually a local image file path",
//! shared by every place that has to tell a genuine clipboard-image drop
//! (iTerm2/Terminal.app substituting a temp-file path for a pasted screenshot)
//! apart from ordinary pasted text.
//!
//! Kept pure and side-effect free on purpose: the shape check
//! ([`local_image_path_from_text`]) never touches the filesystem, so it is
//! assertable without creating temp files, and callers that do need bytes
//! ([`read_local_image_file`]) go through the same bounded reader
//! (`crate::platform::read_limited_reader`) so a mistakenly-matched multi-GB
//! file can never be read in full.

use std::path::{Path, PathBuf};

use crate::protocol::MAX_CLIPBOARD_IMAGE_PAYLOAD;

/// Recognizes an image file extension case-insensitively, normalizing `jpeg`
/// to `jpg` the way the rest of the clipboard-image pipeline expects.
pub(crate) fn recognized_image_extension(extension: &str) -> Option<&'static str> {
    if extension.eq_ignore_ascii_case("png") {
        Some("png")
    } else if extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg") {
        Some("jpg")
    } else if extension.eq_ignore_ascii_case("gif") {
        Some("gif")
    } else if extension.eq_ignore_ascii_case("webp") {
        Some("webp")
    } else if extension.eq_ignore_ascii_case("bmp") {
        Some("bmp")
    } else {
        None
    }
}

fn strip_matching_path_quotes(text: &str) -> &str {
    if text.len() < 2 {
        return text;
    }

    let bytes = text.as_bytes();
    match (bytes.first(), bytes.last()) {
        (Some(b'\''), Some(b'\'')) | (Some(b'"'), Some(b'"')) => &text[1..text.len() - 1],
        _ => text,
    }
}

fn unescape_terminal_drop_path(text: &str) -> String {
    let mut unescaped = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(escaped) = chars.next() {
                unescaped.push(escaped);
            } else {
                unescaped.push(ch);
            }
        } else {
            unescaped.push(ch);
        }
    }
    unescaped
}

/// Decides whether `text` has the exact shape of a single dropped image
/// path: one line, no control characters, an absolute path, and a
/// recognized image extension. Never touches the filesystem — a
/// syntactically valid path to a file that does not exist, or is not a
/// regular file, still passes here and is rejected later by
/// [`read_local_image_file`].
pub(crate) fn local_image_path_from_text(text: &str) -> Option<(PathBuf, &'static str)> {
    let text = text.trim_end_matches(['\r', '\n']);
    if text.is_empty() || text.contains(['\r', '\n']) {
        return None;
    }

    let text = unescape_terminal_drop_path(strip_matching_path_quotes(text));
    let path = PathBuf::from(text);
    if !path.is_absolute() {
        return None;
    }

    let extension = recognized_image_extension(path.extension()?.to_str()?)?;
    Some((path, extension))
}

/// Whether `path` resolves inside a location the OS or a terminal is known to
/// drop clipboard-image temp files into, and if so, the canonicalized path
/// that was actually checked. Deliberately narrow: this is an extra gate for
/// interception paths that trigger on ordinary paste content rather than a
/// dedicated keybinding, so a stray absolute path to an image the user
/// actually meant to paste as text (e.g. a path under their home directory
/// or the repo) is left alone.
///
/// Callers must read the *returned* path, not the one they passed in. A
/// caller that re-checks this gate and then reopens the original `path` has
/// a symlink-swap TOCTOU window between the two calls — the checked target
/// and the read target would not be provably the same file. Handing back the
/// resolved path collapses that gap to a single resolve-then-open pair, the
/// same residual window every path-based check has and no wider.
pub(crate) fn recognized_image_drop_location(path: &Path) -> Option<PathBuf> {
    let canonical_path = path.canonicalize().ok()?;
    let canonical_temp_dir = std::env::temp_dir().canonicalize().ok()?;
    if canonical_path.starts_with(&canonical_temp_dir) {
        Some(canonical_path)
    } else {
        None
    }
}

/// Reads an already-shape-validated candidate path, bounded by
/// `MAX_CLIPBOARD_IMAGE_PAYLOAD`. Returns `None` for anything that is not a
/// plain, readable, in-budget file — including an oversized one, which is
/// logged rather than silently dropped since it is the one rejection with a
/// distinct, actionable cause.
pub(crate) fn read_local_image_file(
    path: &Path,
    extension: &'static str,
) -> Option<crate::platform::ClipboardImage> {
    // Open first and take every fact from the fd: a metadata()-then-open()
    // pair on the same path string is two independent syscalls, and the file
    // can be swapped between them. Everything after this line describes the
    // one file this handle is pinned to.
    let file = std::fs::File::open(path).ok()?;
    read_image_from_open_file(file, extension)
}

/// Bridges an already-open handle, taking `is_file` and the byte budget from
/// the fd itself so no decision is made about a path that could have been
/// swapped since the open.
fn read_image_from_open_file(
    file: std::fs::File,
    extension: &'static str,
) -> Option<crate::platform::ClipboardImage> {
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }

    let bytes =
        match crate::platform::read_limited_reader(file, MAX_CLIPBOARD_IMAGE_PAYLOAD).ok()? {
            crate::platform::LimitedRead::Complete(bytes) => bytes,
            crate::platform::LimitedRead::Empty => return None,
            crate::platform::LimitedRead::Oversized => {
                tracing::warn!(
                    max = MAX_CLIPBOARD_IMAGE_PAYLOAD,
                    "local image file candidate is too large to bridge"
                );
                return None;
            }
        };

    Some(crate::platform::ClipboardImage { bytes, extension })
}

/// The authoritative gate + read for a terminal-substituted clipboard-image
/// path, designed to hold under a hostile shared temp dir: open the file
/// FIRST, then prove that the very inode behind the open fd is what a
/// temp-dir-contained canonical resolution of the path currently names.
/// A symlink swapped in after [`recognized_image_drop_location`]'s advisory
/// on-loop check (or after the open) changes what the canonical path resolves
/// to, the (dev, ino) comparison fails, and the paste is refused — the bytes
/// shipped to the remote can only ever be a file that is provably inside the
/// OS temp dir at the moment it is read.
pub(crate) fn read_verified_image_drop_file(
    path: &Path,
    extension: &'static str,
) -> Option<crate::platform::ClipboardImage> {
    use std::os::unix::fs::MetadataExt;

    let file = std::fs::File::open(path).ok()?;
    let fd_metadata = file.metadata().ok()?;
    if !fd_metadata.is_file() {
        return None;
    }
    let canonical_path = recognized_image_drop_location(path)?;
    let canonical_metadata = std::fs::metadata(&canonical_path).ok()?;
    if canonical_metadata.dev() != fd_metadata.dev()
        || canonical_metadata.ino() != fd_metadata.ino()
    {
        tracing::warn!(
            path = %path.display(),
            "image drop candidate changed identity between open and containment check; refusing to bridge"
        );
        return None;
    }
    read_image_from_open_file(file, extension)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_supported_extensions_case_insensitively() {
        assert_eq!(recognized_image_extension("PNG"), Some("png"));
        assert_eq!(recognized_image_extension("JPEG"), Some("jpg"));
        assert_eq!(recognized_image_extension("jpg"), Some("jpg"));
        assert_eq!(recognized_image_extension("Gif"), Some("gif"));
        assert_eq!(recognized_image_extension("WEBP"), Some("webp"));
        assert_eq!(recognized_image_extension("bmp"), Some("bmp"));
        assert_eq!(recognized_image_extension("txt"), None);
    }

    #[test]
    fn accepts_a_single_line_absolute_image_path() {
        let (path, extension) = local_image_path_from_text("/var/folders/x/clip.png\n").unwrap();
        assert_eq!(path, PathBuf::from("/var/folders/x/clip.png"));
        assert_eq!(extension, "png");
    }

    #[test]
    fn accepts_quoted_and_escaped_paths() {
        let (path, _) = local_image_path_from_text("'/tmp/a b.png'").unwrap();
        assert_eq!(path, PathBuf::from("/tmp/a b.png"));

        let (path, _) = local_image_path_from_text("/tmp/a\\ b.png").unwrap();
        assert_eq!(path, PathBuf::from("/tmp/a b.png"));
    }

    #[test]
    fn rejects_multi_line_text() {
        assert!(local_image_path_from_text("/tmp/a.png\nextra line").is_none());
        assert!(local_image_path_from_text("some\ntext").is_none());
    }

    #[test]
    fn rejects_relative_paths() {
        assert!(local_image_path_from_text("relative.png").is_none());
    }

    #[test]
    fn rejects_non_image_extensions() {
        assert!(local_image_path_from_text("/tmp/file.txt").is_none());
    }

    #[test]
    fn rejects_empty_text() {
        assert!(local_image_path_from_text("\n").is_none());
        assert!(local_image_path_from_text("").is_none());
    }

    #[test]
    fn read_local_image_file_rejects_missing_file() {
        assert!(read_local_image_file(Path::new("/nonexistent/path.png"), "png").is_none());
    }

    #[test]
    fn read_local_image_file_rejects_directory() {
        assert!(read_local_image_file(Path::new("/tmp"), "png").is_none());
    }

    #[test]
    fn read_local_image_file_reads_existing_file() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "herdr-image-path-test-{}-{}.png",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"bytes").unwrap();
        let image = read_local_image_file(&path, "png").unwrap();
        assert_eq!(image.bytes, b"bytes");
        assert_eq!(image.extension, "png");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn recognized_image_drop_location_accepts_temp_dir() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "herdr-image-path-loc-test-{}-{}.png",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"bytes").unwrap();
        let canonical = recognized_image_drop_location(&path).unwrap();
        assert_eq!(canonical, path.canonicalize().unwrap());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn recognized_image_drop_location_rejects_outside_temp_dir() {
        assert!(
            recognized_image_drop_location(Path::new("/nonexistent/not-a-real-path.png")).is_none()
        );
    }

    #[test]
    fn recognized_image_drop_location_rejects_an_existing_file_outside_temp_dir() {
        // Unlike the nonexistent-path case above, this file really exists —
        // `canonicalize()` succeeds, so this is the one test that actually
        // exercises the `starts_with` containment check rather than short
        // circuiting on the earlier `.ok()?`.
        let home = std::env::var("HOME").expect("HOME must be set to run this test");
        let path = std::path::PathBuf::from(home).join(format!(
            "herdr-image-path-outside-temp-dir-test-{}-{}.png",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"bytes").unwrap();

        assert!(recognized_image_drop_location(&path).is_none());

        let _ = std::fs::remove_file(&path);
    }

    /// A regression test for the TOCTOU a caller closes by reading through
    /// the *returned* canonical path instead of the one it submitted: a
    /// symlink under the temp dir pointing at a real file also under the
    /// temp dir must resolve to that real file's path, not echo the
    /// submitted symlink path back unchanged. A caller that ignored the
    /// return value and reopened the original symlink path would be exposed
    /// to whatever the link is retargeted to between the two calls; a caller
    /// that reads through the resolved path this function returns is reading
    /// the exact file that was validated.
    #[test]
    fn recognized_image_drop_location_resolves_a_symlink_to_its_real_target() {
        let dir = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let real_target = dir.join(format!("herdr-image-path-toctou-real-{nanos}.png"));
        let link_path = dir.join(format!("herdr-image-path-toctou-link-{nanos}.png"));
        std::fs::write(&real_target, b"real-bytes").unwrap();
        std::os::unix::fs::symlink(&real_target, &link_path).unwrap();

        let resolved = recognized_image_drop_location(&link_path).unwrap();

        assert_ne!(
            resolved, link_path,
            "the returned path must be the resolved target, not the submitted symlink"
        );
        assert_eq!(resolved, real_target.canonicalize().unwrap());
        // Proves the returned path is actually readable as the real file —
        // this is what a caller must read through, never the submitted path.
        let image = read_local_image_file(&resolved, "png").unwrap();
        assert_eq!(image.bytes, b"real-bytes");

        let _ = std::fs::remove_file(&link_path);
        let _ = std::fs::remove_file(&real_target);
    }
}
