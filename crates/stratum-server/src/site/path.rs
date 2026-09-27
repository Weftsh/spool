//! Turning a request path into the tree paths that might answer it.
//!
//! Split out with no I/O in it because it is the security boundary of
//! the whole feature and the only part of serving that can be tested
//! exhaustively. Everything here is a pure function over a string: a
//! guard whose tests need a store is a guard that gets tested once.
//!
//! Two rules do the work.
//!
//! **Segments are decoded after splitting, never before.** A request for
//! `a%2Fb` must be a lookup for one entry literally named `a/b`, which no
//! git tree can contain, and so a miss. Decoding first would turn it into
//! two segments and hand the request a separator it did not have — the
//! classic way a path guard is walked past.
//!
//! **`.` and `..` are refused rather than resolved.** git itself refuses
//! to store either as an entry name, so there is nothing they could
//! legitimately match; refusing is both safer and a clearer answer than
//! normalising and missing.

/// The most segments a request path may have. A site is a directory
/// somebody committed, not an arbitrarily deep probe, and a bound here
/// bounds the tree walk — which costs a store round trip per segment on
/// a cold cache.
pub const MAX_SEGMENTS: usize = 32;

/// The most one path segment may be, in bytes. Longer than any filename
/// git will have taken, short enough that a hostile URL cannot make one
/// lookup expensive.
pub const MAX_SEGMENT: usize = 255;

/// Percent-decode one already-split segment.
///
/// Lossy on invalid UTF-8, matching how the rest of the server reads
/// bytes that should have been text: a name that does not decode cannot
/// match a tree entry either way, so the honest answer is a miss rather
/// than a 400 about encoding.
fn decode(seg: &str) -> String {
    let bytes = seg.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&seg[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A request path, cleaned into a repository-relative path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Clean {
    /// The path is fine. Empty means the site root.
    Ok(String),
    /// Nothing we will look up. The caller answers a 404 without
    /// touching the store — a refused path must not cost a read.
    Refuse,
}

/// Clean a request path.
///
/// Empty segments are dropped, so `//a//b` and `/a/b` are the same
/// request; a trailing slash is *not* preserved here, because whether a
/// directory needs a redirect is a question about the tree and is
/// decided by the caller.
pub fn clean(url_path: &str) -> Clean {
    let mut out: Vec<String> = Vec::new();
    for raw in url_path.split('/') {
        if raw.is_empty() {
            continue;
        }
        if raw.len() > MAX_SEGMENT {
            return Clean::Refuse;
        }
        let seg = decode(raw);
        // After decoding, and therefore covering `%2e%2e` as well as
        // `..` written plainly.
        if seg == "." || seg == ".." {
            return Clean::Refuse;
        }
        if seg.is_empty() || seg.contains('\0') || seg.contains('/') {
            return Clean::Refuse;
        }
        if seg.len() > MAX_SEGMENT {
            return Clean::Refuse;
        }
        if out.len() >= MAX_SEGMENTS {
            return Clean::Refuse;
        }
        out.push(seg);
    }
    Clean::Ok(out.join("/"))
}

/// Did the request end in a slash? Kept separate from [`clean`] because
/// it is about what the browser should be told, not about what is safe.
pub fn had_trailing_slash(url_path: &str) -> bool {
    url_path.ends_with('/')
}

/// The tree paths to try for a cleaned path, in order, first hit wins.
///
/// The order is GitHub Pages', because it is the one people already have
/// working sites written against: the exact file, then the same name
/// with `.html`, then a directory index.
///
/// `/` and any path the request ended a slash on want only the index —
/// asking for `foo/` is asking for a directory, and `foo/.html` is not a
/// thing.
pub fn candidates(clean: &str, trailing_slash: bool) -> Vec<String> {
    if clean.is_empty() {
        return vec!["index.html".to_string()];
    }
    if trailing_slash {
        return vec![format!("{clean}/index.html")];
    }
    vec![
        clean.to_string(),
        format!("{clean}.html"),
        format!("{clean}/index.html"),
    ]
}

/// Where a request for a directory without a trailing slash should be
/// sent, so that relative links inside the page resolve against the
/// directory rather than against its parent.
pub fn redirect_for(clean: &str) -> String {
    format!("/{clean}/")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One helper with both arms, rather than an `ok` that panics on a
    /// refusal and a `refused` beside it: the panic arm of such a helper
    /// is test code no test ever runs, which the coverage gate is right
    /// to notice.
    fn cleaned(p: &str) -> Option<String> {
        match clean(p) {
            Clean::Ok(s) => Some(s),
            Clean::Refuse => None,
        }
    }

    #[track_caller]
    fn ok(p: &str) -> String {
        cleaned(p).unwrap_or_else(|| panic!("{p:?} should be clean"))
    }

    fn refused(p: &str) -> bool {
        cleaned(p).is_none()
    }

    #[test]
    fn a_root_request_is_the_empty_path() {
        assert_eq!(ok("/"), "");
        assert_eq!(ok(""), "");
    }

    #[test]
    fn ordinary_paths_pass_through_with_the_leading_slash_dropped() {
        assert_eq!(ok("/index.html"), "index.html");
        assert_eq!(ok("/assets/app.css"), "assets/app.css");
        assert_eq!(ok("/a/b/c/d.png"), "a/b/c/d.png");
    }

    #[test]
    fn empty_segments_collapse() {
        assert_eq!(ok("//a//b//"), "a/b");
        assert_eq!(ok("/a/"), "a");
    }

    #[test]
    fn a_segment_is_percent_decoded() {
        assert_eq!(ok("/my%20file.html"), "my file.html");
        assert_eq!(ok("/caf%C3%A9/index.html"), "café/index.html");
    }

    /// The rule that matters: decoding happens *after* splitting, so an
    /// encoded separator stays inside one segment and can never add a
    /// path level.
    #[test]
    fn an_encoded_separator_does_not_become_a_separator() {
        assert!(refused("/a%2Fb"), "%2F decodes to `/` inside a segment");
        assert!(refused("/a%2fb"));
    }

    /// Written plainly and encoded, upper and lower case.
    #[test]
    fn dot_segments_are_refused_however_they_are_spelled() {
        for p in [
            "/../etc/passwd",
            "/a/../../b",
            "/%2e%2e/b",
            "/%2E%2E/b",
            "/a/%2e/b",
            "/.",
            "/..",
            "/a/..",
        ] {
            assert!(refused(p), "{p:?} should be refused");
        }
    }

    #[test]
    fn a_nul_byte_is_refused() {
        assert!(refused("/a%00b"));
    }

    /// A `%` that does not begin a valid escape is a literal `%`, not an
    /// error and not a decoded byte. A file really can be named that.
    #[test]
    fn a_malformed_escape_stays_a_literal_percent() {
        assert_eq!(ok("/100%25.html"), "100%.html");
        assert_eq!(ok("/a%zzb"), "a%zzb");
        assert_eq!(ok("/a%b"), "a%b");
        assert_eq!(ok("/%"), "%");
        assert_eq!(ok("/50%off"), "50%off");
    }

    #[test]
    fn an_absurdly_deep_or_long_path_is_refused_without_a_lookup() {
        let deep = "/a".repeat(MAX_SEGMENTS + 1);
        assert!(refused(&deep));
        let long = format!("/{}", "a".repeat(MAX_SEGMENT + 1));
        assert!(refused(&long));
        // …and the boundary itself is fine.
        assert!(!refused(&"/a".repeat(MAX_SEGMENTS)));
        assert!(!refused(&format!("/{}", "a".repeat(MAX_SEGMENT))));
    }

    /// A decoded segment can be longer in bytes than the raw one is in
    /// characters, so the bound is checked on both sides of decoding.
    #[test]
    fn the_length_bound_survives_decoding() {
        // Each `%C3%A9` is 6 raw bytes and 2 decoded ones, so a raw
        // length under the bound can still decode under it — the point
        // is only that neither side is unbounded.
        let s = format!("/{}", "%41".repeat(MAX_SEGMENT));
        assert!(refused(&s), "raw length is checked before decoding");
    }

    #[test]
    fn the_root_asks_only_for_the_index() {
        assert_eq!(candidates("", false), ["index.html"]);
        assert_eq!(candidates("", true), ["index.html"]);
    }

    /// GitHub Pages' order, because people have working sites written
    /// against it.
    #[test]
    fn a_bare_path_tries_the_file_then_html_then_a_directory_index() {
        assert_eq!(
            candidates("about", false),
            ["about", "about.html", "about/index.html"]
        );
    }

    #[test]
    fn a_trailing_slash_asks_only_for_the_directory_index() {
        assert_eq!(candidates("blog", true), ["blog/index.html"]);
    }

    #[test]
    fn a_directory_redirect_keeps_the_path_and_adds_the_slash() {
        assert_eq!(redirect_for("blog"), "/blog/");
        assert_eq!(redirect_for("a/b"), "/a/b/");
    }

    #[test]
    fn trailing_slash_is_read_off_the_raw_request() {
        assert!(had_trailing_slash("/blog/"));
        assert!(!had_trailing_slash("/blog"));
        assert!(had_trailing_slash("/"));
    }
}
