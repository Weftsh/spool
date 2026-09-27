//! Walking a published deploy's tree to find one file.
//!
//! Kept apart from the handler because the walk has a shape worth
//! testing on its own: it costs a store round trip per path segment on a
//! cold cache, it must tell "this is a directory" from "this is not
//! here" (the first is a redirect and the second is a 404), and it must
//! stop at a symlink rather than following one.
//!
//! Symlinks are the sharp one. A git tree can hold mode `120000`, whose
//! blob is a *path* rather than content. Following one would let a
//! committed symlink read something outside the published directory —
//! the tarball-extraction hazard, arriving through the front door
//! instead. They are simply not served.

use stratum_engine::objwrite::{self, hex, OBJ_BLOB, OBJ_TREE};
use stratum_engine::read::LayoutReader;

/// git's mode for a symbolic link. Its blob is a target path.
const MODE_SYMLINK: &str = "120000";
/// git's mode for a submodule. There is nothing here to serve.
const MODE_GITLINK: &str = "160000";

fn is_tree(mode: &str) -> bool {
    mode == "40000" || mode == "040000"
}

/// What a path inside a deploy turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// A regular file: its object id and its bytes.
    File { oid: String, data: Vec<u8> },
    /// A directory. The caller redirects to the trailing-slash form so
    /// relative links inside the page resolve correctly.
    Dir,
    /// Nothing at that path, or something we will not serve — a symlink,
    /// a submodule.
    Missing,
}

/// Resolve `path` under `tree_oid`.
///
/// `path` is expected to be already cleaned by [`super::path::clean`]:
/// no empty, `.` or `..` segments. This function does not re-check that,
/// and it does not need to — a git tree cannot contain an entry named
/// any of them, so an uncleaned path fails to match rather than
/// escaping. The cleaning is a bound on work, not the security boundary
/// on its own.
pub fn find(reader: &LayoutReader, tree_oid: &str, path: &str) -> Result<Found, String> {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return Ok(Found::Dir);
    }
    let mut oid = tree_oid.to_string();
    for (i, part) in parts.iter().enumerate() {
        let (kind, data) = reader.object(&oid)?;
        if kind != OBJ_TREE {
            return Ok(Found::Missing);
        }
        let entries = objwrite::parse_tree(&data)?;
        let Some(e) = entries.into_iter().find(|e| e.name == *part) else {
            return Ok(Found::Missing);
        };
        // Never followed, at any depth: a symlink's blob is a path, and
        // resolving it is how a published directory stops bounding what
        // can be read.
        if e.mode == MODE_SYMLINK || e.mode == MODE_GITLINK {
            return Ok(Found::Missing);
        }
        let child = hex(&e.oid);
        if i == parts.len() - 1 {
            if is_tree(&e.mode) {
                return Ok(Found::Dir);
            }
            let (k, blob) = reader.object(&child)?;
            if k != OBJ_BLOB {
                return Ok(Found::Missing);
            }
            return Ok(Found::File {
                oid: child,
                data: blob,
            });
        }
        if !is_tree(&e.mode) {
            // A path segment that is a file: `a/b` where `a` is a blob.
            return Ok(Found::Missing);
        }
        oid = child;
    }
    unreachable!("the loop returns on its last iteration")
}

/// The content type for a served path, by extension.
///
/// Wider than the table `webassets` uses, because that one only ever
/// sees our own build output and this one sees whatever somebody
/// committed. Anything unrecognised is `application/octet-stream`, which
/// with `X-Content-Type-Options: nosniff` means a browser downloads it
/// rather than guessing — the safe direction when the bytes belong to a
/// stranger.
pub fn content_type(path: &str) -> &'static str {
    // Basename first, then its extension. Splitting the whole path on
    // its last dot happens to give the same answer, but only because
    // `v1.2/README` yields the nonsense extension `2/README` and falls
    // through to the default — right by accident is not right.
    let name = path.rsplit_once('/').map(|(_, n)| n).unwrap_or(path);
    let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    match ext.to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "map" => "application/json",
        "xml" => "application/xml; charset=utf-8",
        "txt" | "md" => "text/plain; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wasm" => "application/wasm",
        "pdf" => "application/pdf",
        "webmanifest" => "application/manifest+json",
        "atom" => "application/atom+xml",
        "rss" => "application/rss+xml",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_common_web_types_are_named_and_the_rest_download() {
        assert_eq!(content_type("index.html"), "text/html; charset=utf-8");
        assert_eq!(content_type("a/b/app.css"), "text/css; charset=utf-8");
        assert_eq!(content_type("app.js"), "text/javascript; charset=utf-8");
        assert_eq!(content_type("logo.svg"), "image/svg+xml");
        assert_eq!(content_type("font.woff2"), "font/woff2");
        assert_eq!(content_type("thing.unknown"), "application/octet-stream");
        assert_eq!(content_type("noextension"), "application/octet-stream");
    }

    /// A committed file may be named anything, including with the
    /// extension in a case nobody expects.
    #[test]
    fn the_extension_is_matched_case_insensitively() {
        assert_eq!(content_type("README.MD"), "text/plain; charset=utf-8");
        assert_eq!(content_type("PHOTO.JPG"), "image/jpeg");
        assert_eq!(content_type("Index.HTML"), "text/html; charset=utf-8");
    }

    /// The extension is the basename's, so a dot in a *directory* name
    /// is never mistaken for one.
    #[test]
    fn a_dot_in_a_directory_name_is_not_the_files_extension() {
        assert_eq!(content_type("v1.2/app.css"), "text/css; charset=utf-8");
        assert_eq!(content_type("v1.2/README"), "application/octet-stream");
        assert_eq!(
            content_type("a.b/c.d/page.html"),
            "text/html; charset=utf-8"
        );
        // A dotfile is a name, not an extension.
        assert_eq!(content_type("assets/.htaccess"), "application/octet-stream");
    }
}
