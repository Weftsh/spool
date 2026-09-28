//! What a repository *is*, beside what is in it.
//!
//! A file tree, a README and a one-line description is what a directory
//! listing looks like. What tells a visitor whether a project is worth
//! their afternoon is the shape of it — what it is written in, what
//! licence it carries, whether it has anywhere to send a bug report,
//! and what words the maintainers chose to be found by. Three of those
//! four are already sitting in the tree we serve and had never been
//! read; the fourth is `repo_topics`.
//!
//! **Nothing here is new git plumbing.** Every fact except the topics
//! comes out of one `LayoutReader` walk of the default branch, the same
//! reader `api::reads` opens for a file listing.
//!
//! ## Visibility
//!
//! This is a new way to leak that a private repository exists, so it
//! does not decide anything about that itself: both routes go through
//! [`crate::app::rest_repo_auth`], which resolves the repository through
//! `repo_or_masked` and answers a stranger asking about a private
//! project exactly as it answers one asking about a repository that was
//! never created. A language bar is a small thing to leak and "acme has
//! a private repository called `payments`" is not.
//!
//! ## What is deliberately *not* here
//!
//! A guess. The licence table reports a licence only when exactly one
//! fingerprint matches, and says `unrecognised` otherwise. Telling
//! somebody a project is MIT when it is AGPL is materially worse than
//! telling them we could not tell — the first is a licence decision
//! made on our word, and people redistribute code on the strength of
//! that badge.

use crate::api::{internal, json_error};
use crate::app::SharedState;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use stratum_control::audit::AuditCtx;
use stratum_control::auth::Scope;
use stratum_control::topics;
use stratum_engine::objwrite::{self, hex, OBJ_BLOB, OBJ_TREE};
use stratum_engine::read::LayoutReader;

// ---------------------------------------------------------------------
// Bounds (I13)
// ---------------------------------------------------------------------

/// How many tree entries one walk will visit.
///
/// The walk inflates every blob it counts — a blob's length is only
/// knowable by decompressing it, as `api::reads` says at more length.
///
/// **The cost is CPU, not store round trips, and that is worth being
/// precise about because the first version of this comment was wrong.**
/// `LayoutReader` fetches segments, not objects, so ten files and ten
/// thousand come out of the same handful of GETs — measured at three
/// store round trips for the seeded fixture, in
/// `a_revalidated_panel_costs_strictly_fewer_store_round_trips`. What
/// is unbounded without this budget is decompression: an
/// attacker-shaped repository is a denial of service with no attacker
/// required.
///
/// When the bound is hit the answer says so (`truncated`) rather than
/// pretending the proportions are of the whole tree; the same choice
/// `last_commit_per_entry` makes, for the same reason.
///
/// Configurable for the same reason `STRATUM_LOG_SCAN_BUDGET` is: at
/// twenty thousand entries the truncation branch is only reachable from
/// a repository nobody will build in a test, and **a limit nothing ever
/// exercises is a limit nobody knows still works.** A test spawns a
/// server with a budget of two and asks about a repository with more
/// files than that. Floored at 1, because a budget of zero would report
/// every repository as truncated and empty.
fn walk_budget() -> usize {
    std::env::var("STRATUM_META_WALK_BUDGET")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(20_000)
        .max(1)
}

/// How deep the walk descends. A tree cannot contain itself — git
/// content-addresses it — but a repository can be a thousand
/// directories deep, and recursion depth is not something to leave to
/// whatever the caller committed.
///
/// Configurable on the same argument as the budget above.
fn walk_depth() -> usize {
    std::env::var("STRATUM_META_WALK_DEPTH")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(32)
        .max(1)
}

/// The largest candidate licence file this will read.
///
/// Refused rather than truncated: a licence identified from the first
/// 256 KiB of a 40 MB file is a licence identified from a fragment
/// somebody chose, which is precisely the input this table must not be
/// steerable by. Over the bound the answer is `unrecognised`.
const LICENSE_BYTES: usize = 256 * 1024;

/// How many licence files the answer will list.
///
/// A multi-licensed project has two or three. A tree with a thousand
/// `LICENSE-*` files is somebody's input, and the response is bounded
/// against it like everything else here.
const MAX_LICENSE_FILES: usize = 8;

/// How many distinct languages the answer carries.
///
/// The client renders a three-colour bar and aggregates the rest, but
/// that is the palette's business and not the API's. This bound exists
/// only so a repository with two thousand file extensions cannot make
/// the response unbounded.
const MAX_LANGUAGES: usize = 24;

// ---------------------------------------------------------------------
// Languages
// ---------------------------------------------------------------------

/// Extension → language, for the languages a bar should count.
///
/// Two decisions are worth stating because they are what makes the bar
/// mean anything.
///
/// **Data and prose are not languages.** Markdown, JSON, YAML, TOML,
/// lockfiles and plain text are excluded, the same call `linguist`
/// makes. Counting them turns nearly every documentation-heavy
/// repository into a "Markdown project", and this one — where the docs
/// and the ledger outweigh several crates — is a good example of how
/// wrong that reads.
///
/// **Unknown extensions are not "Other".** A file this table does not
/// recognise is not counted at all. "Other" in the rendered bar means
/// "the languages ranked below the third", which is a statement about
/// this repository; folding unknown bytes into it would make it a
/// statement about the gaps in this table instead.
const LANGUAGES: &[(&str, &str)] = &[
    ("rs", "Rust"),
    ("go", "Go"),
    ("c", "C"),
    ("h", "C"),
    ("cc", "C++"),
    ("cpp", "C++"),
    ("cxx", "C++"),
    ("hpp", "C++"),
    ("hh", "C++"),
    ("cs", "C#"),
    ("java", "Java"),
    ("kt", "Kotlin"),
    ("kts", "Kotlin"),
    ("swift", "Swift"),
    ("m", "Objective-C"),
    ("mm", "Objective-C++"),
    ("py", "Python"),
    ("rb", "Ruby"),
    ("php", "PHP"),
    ("pl", "Perl"),
    ("lua", "Lua"),
    ("ex", "Elixir"),
    ("exs", "Elixir"),
    ("erl", "Erlang"),
    ("hs", "Haskell"),
    ("ml", "OCaml"),
    ("scala", "Scala"),
    ("clj", "Clojure"),
    ("dart", "Dart"),
    ("zig", "Zig"),
    ("nim", "Nim"),
    ("ts", "TypeScript"),
    ("tsx", "TypeScript"),
    ("js", "JavaScript"),
    ("jsx", "JavaScript"),
    ("mjs", "JavaScript"),
    ("cjs", "JavaScript"),
    ("vue", "Vue"),
    ("svelte", "Svelte"),
    ("astro", "Astro"),
    ("html", "HTML"),
    ("css", "CSS"),
    ("scss", "SCSS"),
    ("sh", "Shell"),
    ("bash", "Shell"),
    ("zsh", "Shell"),
    ("ps1", "PowerShell"),
    ("sql", "SQL"),
    ("tf", "HCL"),
    ("hcl", "HCL"),
    ("proto", "Protocol Buffers"),
    ("r", "R"),
    ("jl", "Julia"),
    ("f90", "Fortran"),
    ("asm", "Assembly"),
    ("s", "Assembly"),
];

/// The language a filename counts towards, if any.
///
/// Extension only, and lowercased. A `Makefile` or a `Dockerfile` has
/// no extension and is deliberately uncounted: recognising them would
/// mean a second table keyed on whole names, and neither is a language
/// anybody is deciding about a project on.
///
/// A dotfile is not an extension. `.gitignore` splits to `("", "gitignore")`
/// on the last dot, and treating `gitignore` as an extension is how
/// `.rs`-the-dotfile would be counted as Rust. A name whose only dot is
/// the first character has no extension at all.
pub fn language_of(name: &str) -> Option<&'static str> {
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    let ext = ext.to_ascii_lowercase();
    LANGUAGES
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, lang)| *lang)
}

/// One language and how many bytes of it there are.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Language {
    pub name: String,
    pub bytes: u64,
}

/// Byte counts, biggest first, bounded.
///
/// Ties break on the name rather than on whatever order the walk
/// happened to produce. That is not tidiness: an unstable order means a
/// repository with two equal languages renders its bar in a different
/// order on every request, and the colours — which are assigned by rank
/// — swap with it.
pub fn rank(counts: &std::collections::HashMap<String, u64>) -> Vec<Language> {
    let mut out: Vec<Language> = counts
        .iter()
        .map(|(name, bytes)| Language {
            name: name.clone(),
            bytes: *bytes,
        })
        .collect();
    out.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
    out.truncate(MAX_LANGUAGES);
    out
}

// ---------------------------------------------------------------------
// Licences
// ---------------------------------------------------------------------

/// A licence text reduced to the only thing a fingerprint may depend on.
///
/// Lowercased, and every character that is not an ASCII letter or digit
/// becomes a single space. That erases, in one step, everything that
/// legitimately varies between two copies of the same licence: line
/// wrapping, the copyright line, `(C)` versus `©`, curly versus straight
/// quotes, CRLF, and the `and/or` in ISC. What it leaves is the
/// sequence of words, which is the part the SPDX text actually fixes.
///
/// Written as a whole-string transform rather than a matcher because
/// the marks in [`LICENSES`] are normalised by eye to the same rule,
/// and a test asserts that every mark survives its own normalisation
/// unchanged — so a mark with a comma in it fails the suite rather than
/// silently never matching.
pub fn normalize_license(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = true;
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            space = false;
        } else if !space {
            out.push(' ');
            space = true;
        }
    }
    out.trim_end().to_string()
}

/// One SPDX identifier and the phrases that identify it.
pub struct LicenseFingerprint {
    pub spdx: &'static str,
    pub name: &'static str,
    /// Every one of these must appear in the normalised text.
    pub marks: &'static [&'static str],
    /// None of these may appear. Present only where two licences in
    /// this table are in a subset relationship — LGPL-3.0 quotes the
    /// GPL-3.0 title and version line verbatim, and BSD-3-Clause is
    /// BSD-2-Clause plus a clause — where matching on presence alone
    /// would make both fire and the answer collapse to "unrecognised"
    /// for a document that is unambiguous to a reader.
    pub absent: &'static [&'static str],
}

/// The table.
///
/// Each mark is a phrase from the canonical SPDX text, normalised by
/// the rule above. Three properties are what make this worth trusting
/// rather than a lookup that happens to work on the files we tried:
///
/// 1. **Every mark must be present.** A single distinctive sentence
///    would match a blog post quoting it; two or three sentences from
///    different parts of a document do not co-occur by accident.
/// 2. **Exactly one entry may match.** A text that fingerprints as two
///    licences is `unrecognised`, not the first hit. Ordering therefore
///    cannot decide an answer, which is what keeps adding a row from
///    silently reclassifying an existing one.
/// 3. **The marks avoid everything that varies.** No copyright holder,
///    no year, no address — the parts of a licence file that differ
///    between two projects under the identical licence.
///
/// The honest limit: this identifies the *unmodified* text. A licence
/// with a clause struck out still matches, because the removed clause
/// is not a mark. That is the same limitation `licensee` has, and it is
/// why the field is called a detection and the UI links to the file.
pub const LICENSES: &[LicenseFingerprint] = &[
    LicenseFingerprint {
        spdx: "MIT",
        name: "MIT License",
        marks: &[
            "permission is hereby granted free of charge to any person obtaining a copy",
            "the software is provided as is without warranty of any kind express or implied",
        ],
        absent: &[],
    },
    LicenseFingerprint {
        spdx: "Apache-2.0",
        name: "Apache License 2.0",
        marks: &["apache license", "version 2 0 january 2004"],
        absent: &[],
    },
    LicenseFingerprint {
        spdx: "BSD-2-Clause",
        name: "BSD 2-Clause \"Simplified\" License",
        marks: &[
            "redistribution and use in source and binary forms with or without modification are permitted provided that the following conditions are met",
            "redistributions in binary form must reproduce the above copyright notice",
        ],
        absent: &["endorse or promote products derived from this software"],
    },
    LicenseFingerprint {
        spdx: "BSD-3-Clause",
        name: "BSD 3-Clause \"New\" or \"Revised\" License",
        marks: &[
            "redistribution and use in source and binary forms with or without modification are permitted provided that the following conditions are met",
            "nor the names of its contributors may be used to endorse or promote products derived from this software",
        ],
        absent: &[],
    },
    LicenseFingerprint {
        spdx: "ISC",
        name: "ISC License",
        marks: &[
            "permission to use copy modify and or distribute this software for any purpose with or without fee is hereby granted",
        ],
        absent: &[],
    },
    LicenseFingerprint {
        spdx: "GPL-2.0",
        name: "GNU General Public License v2.0",
        marks: &["gnu general public license", "version 2 june 1991"],
        absent: &["lesser general public license", "affero general public license"],
    },
    LicenseFingerprint {
        spdx: "GPL-3.0",
        name: "GNU General Public License v3.0",
        marks: &["gnu general public license", "version 3 29 june 2007"],
        absent: &["lesser general public license", "affero general public license"],
    },
    LicenseFingerprint {
        spdx: "LGPL-3.0",
        name: "GNU Lesser General Public License v3.0",
        marks: &["gnu lesser general public license", "version 3 29 june 2007"],
        absent: &[],
    },
    LicenseFingerprint {
        spdx: "AGPL-3.0",
        name: "GNU Affero General Public License v3.0",
        marks: &[
            "gnu affero general public license",
            "version 3 19 november 2007",
        ],
        absent: &[],
    },
    LicenseFingerprint {
        spdx: "MPL-2.0",
        name: "Mozilla Public License 2.0",
        marks: &["mozilla public license version 2 0"],
        absent: &[],
    },
    LicenseFingerprint {
        spdx: "Unlicense",
        name: "The Unlicense",
        marks: &["this is free and unencumbered software released into the public domain"],
        absent: &[],
    },
    LicenseFingerprint {
        spdx: "CC0-1.0",
        name: "Creative Commons Zero v1.0 Universal",
        marks: &["creative commons legal code", "cc0 1 0 universal"],
        absent: &[],
    },
];

/// The SPDX identifier for a licence file's bytes, or `None`.
///
/// `None` means "unrecognised", and it is the answer for every case the
/// table is not certain about: no match, more than one match, bytes
/// that are not UTF-8, and a file past [`LICENSE_BYTES`]. There is no
/// "closest match" path and there must not be one — the value of this
/// field is entirely in a reader being able to believe it.
pub fn detect_license(bytes: &[u8]) -> Option<&'static LicenseFingerprint> {
    if bytes.len() > LICENSE_BYTES {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let norm = normalize_license(text);
    let mut hit: Option<&'static LicenseFingerprint> = None;
    for f in LICENSES {
        let matches =
            f.marks.iter().all(|m| norm.contains(m)) && !f.absent.iter().any(|m| norm.contains(m));
        if matches {
            if hit.is_some() {
                // Two fingerprints, one file. A reader looking at it
                // would have to decide, and so must we — by declining.
                return None;
            }
            hit = Some(f);
        }
    }
    hit
}

/// Whether a filename is a licence file **at all**.
///
/// Deliberately broad: `LICENSE`, `LICENCE`, `COPYING`, any of them with
/// a `.md`/`.txt`/`.rst` suffix, and any of them with a trailing
/// discriminator — `LICENSE-MIT`, `LICENSE-APACHE`, `COPYING.LESSER`.
///
/// **This was narrow and it was wrong, and a browser found it.** The
/// narrow version recognised only the undecorated names, so
/// `rust-lang/rust` — `LICENSE-APACHE` beside `LICENSE-MIT`, which is
/// *the* convention across the Rust ecosystem — matched nothing and the
/// panel rendered no licence row at all. A dual-licensed project
/// reading as unlicensed is precisely the misreading this feature
/// exists to prevent, and it is worse than the guess it was avoiding.
///
/// Breadth here is safe because it decides only *how many* licence
/// files there are. Naming one still requires exactly one, which is
/// what [`fingerprintable`] is for. GitHub answers this case with
/// "Apache-2.0 and 2 other licenses found"; we answer "3 licenses
/// found" and list them, which is the same honesty without picking a
/// primary.
pub fn is_license_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    // A licence file is never source or markup, and this is where the
    // two tables meet: `license.html` is a web page and `license.rs` is
    // code somebody named oddly, while `COPYING.LESSER` and
    // `LICENSE-APACHE-2.0.txt` are neither and are licence files. Doing
    // it this way round — reject what is definitely something else,
    // then match the base — is what lets the base match stay broad
    // enough for the decorated names without swallowing a web page.
    if language_of(&lower).is_some() || lower.ends_with(".html") || lower.ends_with(".htm") {
        return false;
    }
    let stem = match lower.rsplit_once('.') {
        Some((s, "md" | "txt" | "rst")) if !s.is_empty() => s,
        _ => lower.as_str(),
    };
    let base = stem
        .split_once(['-', '_', '.'])
        .map(|(b, _)| b)
        .unwrap_or(stem);
    matches!(base, "license" | "licence" | "copying")
}

// ---------------------------------------------------------------------
// Community files
// ---------------------------------------------------------------------

/// The stem a meta file is recognised by: lowercased, with one
/// documentation extension removed.
///
/// `README.md`, `readme.txt`, `README.rst` and a bare `README` are one
/// file to a reader and have to be one file here. `security.yml` is a
/// workflow that happens to share a word, and is not.
///
/// Extracted so that [`community_kind`] and [`is_readme_name`] cannot
/// drift into two sets of naming rules. The client is promised it never
/// has to know these rules — it is handed a stable kind and a path —
/// and a promise like that is only worth as much as there being one
/// copy of the rule to keep correct.
fn meta_stem(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    match lower.rsplit_once('.') {
        Some((s, "md" | "txt" | "rst")) => s.to_string(),
        _ => lower,
    }
}

/// Which community file a name is, if it is one.
///
/// The three GitHub surfaces as "community health": how to contribute,
/// how to behave, and how to report something you should not shout
/// about. The key returned is the stable JSON field name, so the client
/// never string-matches on a filename.
pub fn community_kind(name: &str) -> Option<&'static str> {
    // Both spellings, because both are in the wild and a project that
    // used hyphens does not have a *less* real code of conduct.
    match meta_stem(name).as_str() {
        "contributing" => Some("contributing"),
        "code_of_conduct" | "code-of-conduct" => Some("code_of_conduct"),
        "security" => Some("security"),
        _ => None,
    }
}

/// Whether a filename is this project's README.
///
/// Same naming rule as the community files above — [`meta_stem`] is the
/// single copy of it — but reported as its own field rather than as a
/// fourth `kind`, for a reason that is about what the client does with
/// it and not about tidiness. The other three are only ever *linked*:
/// the About rail shows a row and the row goes to the file. A README is
/// the page itself; the client fetches its bytes and renders them. So
/// it is a scalar, and "this project has no README" is `null` rather
/// than an absence from a list.
///
/// That distinction is the whole point of shipping this at all. The
/// About rail renders all six health rows present-or-absent, because a
/// row GitHub silently omits makes an incomplete project look identical
/// to a complete one. Absence is a fact here, so it has to be on the
/// wire as one — and `null` is that fact where `""` would be a file
/// named nothing.
pub fn is_readme_name(name: &str) -> bool {
    meta_stem(name) == "readme"
}

// ---------------------------------------------------------------------
// The walk
// ---------------------------------------------------------------------

/// Everything one pass over the tree produces.
#[derive(Debug, Default)]
struct Walk {
    counts: std::collections::HashMap<String, u64>,
    /// Whether the walk stopped on its budget. The proportions are then
    /// of what was measured, and the answer says so.
    truncated: bool,
    /// `(path, oid)` for every licence-shaped file at the root or in
    /// `.github/`, in the order the walk found them, bounded by
    /// [`MAX_LICENSE_FILES`]. One means it can be fingerprinted; more
    /// means the project is multi-licensed and is reported as such.
    licenses: Vec<(String, String)>,
    /// `(kind, path)` for each community file found.
    community: Vec<(&'static str, String)>,
    /// The path of this project's README, if it has one. `None` is the
    /// answer the About rail renders as an absent row, so it must stay
    /// distinguishable from a path — never an empty string.
    readme: Option<String>,
}

/// The licence as the response renders it.
#[derive(Debug, Clone, serde::Serialize)]
struct LicenseView {
    /// The one licence file, when there is exactly one. `None` when the
    /// project carries several — there is no single file to link to,
    /// and `files` is what a reader needs then.
    path: Option<String>,
    spdx: Option<&'static str>,
    name: Option<&'static str>,
    /// True only when exactly one file was found *and* the table is
    /// certain what it is.
    recognised: bool,
    /// Every licence file found, so a multi-licensed project can list
    /// them rather than being reported as having none. Always at least
    /// one when this object exists.
    files: Vec<String>,
}

/// What the reader decided: the ETag, and the panel unless the caller
/// already has it.
#[derive(Debug)]
struct Answer {
    etag: String,
    /// `None` means the caller's `If-None-Match` matched and the walk
    /// was never run.
    meta: Option<Meta>,
}

/// Every path the panel serialises, and the reason this list exists at
/// all.
///
/// It is hashed into the ETag. That looks like belt-and-braces and is
/// not: it is the fix for a bug that shipped.
///
/// The tag used to be a hash of the commit and the topics alone, on the
/// entirely correct reasoning that the panel's *content* is a pure
/// function of those two. What that reasoning misses is that the panel's
/// **shape** is a function of the server's version, and the two change
/// independently. Adding `readme` to the response did not move the tag,
/// so every browser holding a cached body from the previous deploy sent
/// `If-None-Match`, was told 304, and kept rendering the old body — one
/// with no README path in it — until something pushed to the repository.
/// On a rail that renders each health file present-or-absent, that reads
/// as "this project has no README" on a page showing the README.
///
/// A hand-bumped version constant would have fixed that instance and
/// been forgotten by the next one. This cannot be forgotten:
/// [`tests::the_etag_changes_whenever_the_panels_shape_does`] builds a
/// real response and fails if its path set differs from this list, so
/// changing the panel's shape *requires* editing this, and editing it
/// changes every ETag exactly once — which is precisely the
/// invalidation the new field needs.
///
/// **Paths, not top-level keys.** This was a list of top-level keys,
/// which meant the whole mechanism was blind to every shape change
/// below the first level: a field added to `license`, a renamed key in
/// a `community` entry, `topics` turned from strings into objects. Each
/// of those changes the body and moved no tag — which replays the
/// original bug exactly, a stale 304 rendering the old shape until
/// somebody pushed. A path is written `license.spdx` for a nested key
/// and `community[].kind` for a key inside an array element; an array
/// of scalars is its own leaf, `topics[]`.
///
/// It is still a *list* rather than a hash of the body, because the tag
/// has to be computable **before** the tree walk it exists to avoid.
/// The list is the shape; the walk produces the content.
const META_KEYS: &[&str] = &[
    "community[].kind",
    "community[].path",
    "languages[].bytes",
    "languages[].name",
    "languages_truncated",
    "license.files[]",
    "license.name",
    "license.path",
    "license.recognised",
    "license.spdx",
    "readme",
    "topics[]",
];

/// The ETag for one panel.
///
/// **The panel's content is a pure function of two things** — the commit
/// at `HEAD` and the topic rows — so hashing exactly those is exact
/// rather than heuristic, which is the same property `api::reads` gets
/// by using a blob oid. Everything else the answer contains (languages,
/// licence, community files, the README path) is derived from the tree
/// that commit names: none of them can change without the tree
/// changing, git content-addresses the tree into the commit, and the
/// commit is already here.
///
/// The third input is [`META_KEYS`], which is not about content at all
/// — see its comment for the deploy-skew bug it exists to prevent.
///
/// This exists because the walk is not free: it inflates every blob it
/// counts, and the repository page is the most visited page in the
/// product. Deciding the ETag needs only the manifest the reader has
/// already fetched, so a revisit that has not pushed skips the walk
/// entirely — one store round trip fewer and, more to the point, none
/// of the decompression.
///
/// The saving is asserted as a strict inequality in store round trips
/// rather than as a wall clock, and computing the tag *after* the walk
/// makes that test fail with a message saying so.
///
/// The payload is newline-separated, which is unambiguous because
/// `topics::normalize` refuses a topic containing one — so no two
/// distinct topic sets can render to the same string. An empty commit
/// (a repository with no refs) is a real input, not a missing one: its
/// panel is empty and its topics still change.
fn meta_etag(commit: &str, topics: &[String]) -> String {
    let mut payload = String::from(commit);
    for t in topics {
        payload.push('\n');
        payload.push_str(t);
    }
    // The shape marker, after the topics and behind a separator that
    // cannot occur in either — `topics::normalize` refuses a newline, so
    // nothing above can spell this section and no two distinct inputs
    // can render to one payload.
    payload.push_str("\n\nshape");
    for k in META_KEYS {
        payload.push('\n');
        payload.push_str(k);
    }
    // git's own sha1-of-typed-payload, reused because this crate already
    // depends on it. A second hash function in the tree would be a
    // second thing to keep correct for no gain.
    format!(
        "\"{}\"",
        hex(&objwrite::hash_object(OBJ_BLOB, payload.as_bytes()))
    )
}

/// Everything the route answers with, once the reader has closed.
#[derive(Debug, Default)]
struct Meta {
    counts: std::collections::HashMap<String, u64>,
    truncated: bool,
    license: Option<LicenseView>,
    community: Vec<(&'static str, String)>,
    readme: Option<String>,
}

/// Walk a commit's tree once, collecting everything the meta view needs.
///
/// Depth-first with an explicit stack rather than recursion: the depth
/// bound is then a number this function owns instead of a property of
/// the process's stack, and a repository is somebody else's input.
///
/// Licence and community files are looked for at the **root and in
/// `.github/`** only. A `CONTRIBUTING.md` nine directories down belongs
/// to that directory — hoisting it would attribute somebody's
/// `vendor/foo/CONTRIBUTING.md` to this project, the same mistake
/// hoisting a nested README would be.
fn walk_tree(reader: &LayoutReader, commit: &str) -> Result<Walk, String> {
    let mut out = Walk::default();
    let Some(root) = reader.entry_at(commit, "")? else {
        return Ok(out);
    };
    let mut stack: Vec<(String, String, usize)> = vec![(String::new(), hex(&root.oid), 0)];
    let mut visited = 0usize;
    let budget = walk_budget();
    let depth_cap = walk_depth();

    while let Some((dir, oid, depth)) = stack.pop() {
        let (kind, data) = reader.object(&oid)?;
        if kind != OBJ_TREE {
            continue;
        }
        for e in objwrite::parse_tree(&data)? {
            if visited >= budget {
                out.truncated = true;
                return Ok(out);
            }
            visited += 1;
            let path = if dir.is_empty() {
                e.name.clone()
            } else {
                format!("{dir}/{}", e.name)
            };
            let is_tree = e.mode == "40000" || e.mode == "040000";
            if is_tree {
                if depth + 1 < depth_cap {
                    stack.push((path, hex(&e.oid), depth + 1));
                } else {
                    // Not silently skipped: the bar would be of a
                    // subset and would not say so.
                    out.truncated = true;
                }
                continue;
            }
            // A gitlink has no blob here to read and a symlink's
            // "content" is a path, not source. Neither is bytes of code.
            if e.mode != "100644" && e.mode != "100755" {
                continue;
            }
            let meta_dir = dir.is_empty() || dir == ".github";
            if meta_dir && is_license_name(&e.name) && out.licenses.len() < MAX_LICENSE_FILES {
                // Collected rather than overwritten: a repository
                // carrying two licence files is multi-licensed, and the
                // answer must not depend on the order the walk produced
                // them. Bounded like everything else here — a tree with
                // a thousand `LICENSE-*` files is somebody's input.
                out.licenses.push((path.clone(), hex(&e.oid)));
            }
            if meta_dir && out.readme.is_none() && is_readme_name(&e.name) {
                // First sighting wins, and first sighting *is* root
                // before `.github/` without a tie-break to get wrong:
                // the walk starts at the root, and every file entry of
                // a directory is visited before any of its
                // subdirectories is popped off the stack. So a root
                // `README.md` is always seen before `.github/README.md`
                // — root beating `.github/` is the same precedence the
                // community files above get, arrived at by the same
                // walk order.
                //
                // The rules diverge *within* the root, and that is
                // worth knowing before you read one off the other. The
                // community block has an `else if dir.is_empty()` arm,
                // so a later root sighting replaces an earlier one and
                // a project carrying both `CONTRIBUTING.md` and
                // `CONTRIBUTING.txt` reports the **last**. Here there
                // is no such arm, so `README` + `README.md` reports the
                // **first**. Within one directory git orders a tree by
                // name, so `README` wins there.
                //
                // Both are arbitrary and both are *stable*, which is
                // the property that matters: the alternative is a path
                // that changes between two requests answering the same
                // commit, behind an ETag that says nothing has changed.
                out.readme = Some(path.clone());
            }
            if meta_dir {
                if let Some(kind) = community_kind(&e.name) {
                    // Root wins over `.github/`, and the first sighting
                    // of a kind wins over a later one.
                    if !out.community.iter().any(|(k, _)| *k == kind) {
                        out.community.push((kind, path.clone()));
                    } else if dir.is_empty() {
                        out.community.retain(|(k, _)| *k != kind);
                        out.community.push((kind, path.clone()));
                    }
                }
            }
            if let Some(lang) = language_of(&e.name) {
                // The only way to learn a blob's length is to inflate
                // it; `api::reads` says the same at more length and
                // records the same finding — the size is in the pack
                // entry header, and an engine-level `object_size` that
                // read the header and stopped would answer without
                // decompressing anything. That is the fix worth making,
                // and it is why this walk is bounded until it is.
                if let Ok((k, blob)) = reader.object(&hex(&e.oid)) {
                    if k == OBJ_BLOB {
                        *out.counts.entry(lang.to_string()).or_insert(0) += blob.len() as u64;
                    }
                }
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------

/// `GET /v1/orgs/:org/repos/:repo/meta`
///
/// Readable by anyone who may read the repository, a signed-out visitor
/// included — this is the panel a stranger is deciding on. Masked
/// exactly as the code is for anyone who may not.
pub async fn get(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, repo_row, _) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let topics = match topics::list(&state.db, &repo_row.id) {
        Ok(t) => t,
        Err(e) => return internal(e),
    };
    let prefix = repo_row.prefix().as_str().to_string();
    let if_none_match = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let for_tag = topics.clone();
    let walked = crate::api::reads::with_reader(&state, prefix, move |reader| {
        // `HEAD`, which is what every other read here defaults to and
        // is the manifest's own idea of the default branch. Resolving
        // `repos.default_branch` instead and falling back to `HEAD`
        // was the first shape of this and was worse twice: it holds two
        // opinions about which branch a repository's front page is,
        // and the fallback arm is only reachable from a row whose
        // configured default has been deleted out from under it — an
        // untestable branch guarding against a disagreement that only
        // exists because the branch is there.
        //
        // An empty repository is not an error, and resolves to nothing.
        // It has no languages and no licence, and saying so is the
        // correct answer for a repository created a minute ago.
        let commit = reader.resolve_rev("HEAD")?.unwrap_or_default();
        // Decided *before* the walk, which is the whole point: a repeat
        // view costs the manifest read the reader has already done and
        // one topics query, instead of reading every blob in the tree
        // again to measure it.
        let etag = meta_etag(&commit, &for_tag);
        if if_none_match.as_deref() == Some(etag.as_str()) {
            return Ok(Answer { etag, meta: None });
        }
        if commit.is_empty() {
            return Ok(Answer {
                etag,
                meta: Some(Meta::default()),
            });
        }
        let walk = walk_tree(reader, &commit)?;
        // Read the licence inside the same reader: one more object read
        // rather than a second reader and a second manifest fetch.
        let files: Vec<String> = walk.licenses.iter().map(|(p, _)| p.clone()).collect();
        let license = match walk.licenses.as_slice() {
            // No licence file at all: the row is absent, not empty.
            // GitHub omits it too, and a rail of "None" placeholders
            // reads as an unfinished product rather than a young
            // project.
            [] => None,
            // Exactly one, so it can be identified — or honestly not.
            [(path, oid)] => {
                let bytes = reader
                    .object(oid)
                    .ok()
                    .filter(|(k, _)| *k == OBJ_BLOB)
                    .map(|(_, b)| b)
                    .unwrap_or_default();
                let hit = detect_license(&bytes);
                Some(LicenseView {
                    path: Some(path.clone()),
                    spdx: hit.map(|f| f.spdx),
                    name: hit.map(|f| f.name),
                    recognised: hit.is_some(),
                    files,
                })
            }
            // Several: the project is multi-licensed. Naming one of
            // them would be the most misleading answer available, and
            // saying nothing — which this did until a browser pass
            // looked at `rust-lang/rust` — makes a dual-licensed
            // project read as unlicensed. So: neither. The count and
            // the files, and the reader decides.
            _ => Some(LicenseView {
                path: None,
                spdx: None,
                name: None,
                recognised: false,
                files,
            }),
        };
        Ok(Answer {
            etag,
            meta: Some(Meta {
                counts: walk.counts,
                truncated: walk.truncated,
                license,
                community: walk.community,
                readme: walk.readme,
            }),
        })
    })
    .await;
    let answer = match walked {
        Ok(w) => w,
        Err(e) => return crate::api::reads::err_to_response(e),
    };

    // `no-cache` rather than a max-age: it means "revalidate every
    // time", not "do not store". A stale language bar is harmless and a
    // stale *licence* is not, so the browser always asks — and almost
    // always gets 304 and its own copy back. `private` keeps a shared
    // cache out of it, because the same URL is answered or masked
    // depending on who is asking.
    let common = [
        (header::ETAG, answer.etag.clone()),
        (header::CACHE_CONTROL, "private, no-cache".to_string()),
        (header::VARY, "Authorization, Cookie".to_string()),
    ];
    let Some(meta) = answer.meta else {
        return (StatusCode::NOT_MODIFIED, common).into_response();
    };

    (StatusCode::OK, common, Json(body(&topics, &meta))).into_response()
}

/// The panel, as JSON.
///
/// A function rather than an expression inside the handler so that a
/// test can hold the finished object and compare its path set against
/// [`META_KEYS`]. That comparison is the whole enforcement mechanism for
/// the ETag's shape marker: without it, `META_KEYS` is a list somebody
/// has to remember to edit, and the bug it exists to prevent is exactly
/// a thing somebody did not remember.
fn body(topics: &[String], meta: &Meta) -> serde_json::Value {
    let mut community: Vec<serde_json::Value> = meta
        .community
        .iter()
        .map(|(kind, path)| serde_json::json!({ "kind": kind, "path": path }))
        .collect();
    community.sort_by(|a, b| a["kind"].as_str().cmp(&b["kind"].as_str()));

    serde_json::json!({
        "topics": topics,
        "languages": rank(&meta.counts),
        "languages_truncated": meta.truncated,
        "license": meta.license,
        "community": community,
        // The path, not a flag and not a filename the client has to
        // reassemble: it links straight to what the walk found, and
        // stays out of the business of knowing that a README can be
        // `.rst` or live in `.github/`. `null` when there is none.
        "readme": meta.readme,
    })
}

#[derive(Deserialize)]
pub struct TopicsBody {
    pub topics: Vec<String>,
}

/// `PUT /v1/orgs/:org/repos/:repo/topics`
///
/// The whole set, replaced. Write access, because the topics are a
/// claim the project makes about itself.
pub async fn put_topics(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<TopicsBody>,
) -> Response {
    let (org_row, repo_row, principal) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoWrite) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let actx = AuditCtx::of(&org_row.id, Some(&principal));
    match topics::set(&state.db, &repo_row.id, &body.topics, &actx) {
        Ok(stored) => Json(serde_json::json!({ "topics": stored })).into_response(),
        // Every shape rule in `topics` is about the caller's input, and
        // a rejected topic is a 400 they can fix. The database errors
        // this could otherwise surface are already impossible by then:
        // validation runs before the transaction opens.
        Err(e) if e.starts_with("set topics:") => internal(e),
        Err(e) => json_error(StatusCode::BAD_REQUEST, e),
    }
}

#[cfg(test)]
mod tests {

    /// The ETag moves whenever the panel's shape does, and this test is
    /// the only thing that makes that true.
    ///
    /// The bug it pins shipped, and it is worth spelling out because it
    /// is invisible to every other test in this file. `/meta` grew a
    /// `readme` field. The ETag is a hash of the commit and the topics,
    /// neither of which changed — so a browser holding a body cached
    /// from the previous deploy sent `If-None-Match`, was answered 304,
    /// and went on rendering a body with no README path in it until
    /// somebody pushed to the repository. The About rail draws each
    /// health file present-or-absent, so what a reader saw was
    /// "Readme — None" on a page displaying the README.
    ///
    /// Every server-side test passed throughout: they all fetch without
    /// a validator and get a fresh body. The failure only exists across
    /// two *versions* of the server, which no single-version test can
    /// stage.
    ///
    /// So the defence is structural instead. `META_KEYS` is hashed into
    /// the tag, and this test fails if the response grows, loses or
    /// renames a field *anywhere* in the body without that list being
    /// edited to match. Editing it changes every ETag exactly once,
    /// which is the invalidation the new field needed.
    ///
    /// It walks paths rather than top-level keys because the top level
    /// was not enough: a field added to `LicenseView`, a renamed key in
    /// a `community` or `languages` entry, or `topics` turned from
    /// strings into objects all change the body and would all have left
    /// the tag alone. The old test's failure message asked the next
    /// person to bump `META_KEYS` by hand for those — which is the same
    /// "somebody has to remember" mechanism the constant exists to
    /// replace.
    ///
    /// The fixture is a *fully populated* `Meta` rather than
    /// `Meta::default()`, because a `None` sub-object serialises as
    /// `null` and hides every path underneath it: with the default the
    /// walk could only ever see `license`, and the nested half of the
    /// contract would be unenforced while looking enforced.
    #[test]
    fn the_etag_changes_whenever_the_panels_shape_does() {
        let rendered = body(&["git".into()], &fully_populated_meta());
        let mut got = shape_paths(&rendered);
        got.sort();

        let mut want: Vec<String> = META_KEYS.iter().map(|s| (*s).to_string()).collect();
        want.sort();

        assert_eq!(
            got, want,
            "the panel's paths and META_KEYS disagree.\n\n\
             META_KEYS is hashed into the ETag so that changing the \
             panel's shape invalidates the bodies browsers cached from \
             the previous deploy. Add the new path to META_KEYS (or \
             remove the one that went) and this passes.\n\n\
             Paths are spelled `license.spdx` for a nested key, \
             `community[].kind` for a key inside an array element, and \
             `topics[]` for an array of scalars.\n\n\
             If the difference is a path that *should* still be there, \
             the fixture is what is wrong: `fully_populated_meta` has \
             to populate every optional sub-object, or a `None` \
             serialises as `null` and hides everything below it."
        );
    }

    /// Every path in a rendered panel, `a.b[].c` style.
    ///
    /// A `null` is a leaf like any other scalar — it is a value the
    /// shape has, not an absence — which is why the fixture above has
    /// to fill in the optional sub-objects rather than relying on this
    /// to see through them.
    fn shape_paths(v: &serde_json::Value) -> Vec<String> {
        fn walk(v: &serde_json::Value, prefix: &str, out: &mut Vec<String>) {
            match v {
                serde_json::Value::Object(map) => {
                    for (k, val) in map {
                        let p = if prefix.is_empty() {
                            k.clone()
                        } else {
                            format!("{prefix}.{k}")
                        };
                        walk(val, &p, out);
                    }
                }
                serde_json::Value::Array(items) => {
                    let p = format!("{prefix}[]");
                    // An empty array still has a path: it is how an
                    // array of scalars is spelled, and it keeps the
                    // failure message pointing at the fixture rather
                    // than silently dropping a whole branch.
                    if items.is_empty() {
                        out.push(p);
                    } else {
                        for it in items {
                            walk(it, &p, out);
                        }
                    }
                }
                _ => out.push(prefix.to_string()),
            }
        }
        let mut out = Vec::new();
        walk(v, "", &mut out);
        out.sort();
        out.dedup();
        out
    }

    /// A `Meta` with every optional sub-object present, so that every
    /// path the panel can carry is in the rendered body.
    fn fully_populated_meta() -> Meta {
        Meta {
            counts: std::collections::HashMap::from([("Rust".to_string(), 12u64)]),
            truncated: true,
            license: Some(LicenseView {
                path: Some("LICENSE".into()),
                spdx: Some("MIT"),
                name: Some("MIT License"),
                recognised: true,
                files: vec!["LICENSE".into()],
            }),
            community: vec![("code_of_conduct", "CODE_OF_CONDUCT.md".into())],
            readme: Some("README.md".into()),
        }
    }

    /// The walk sees below the top level, which is the whole point of
    /// spelling `META_KEYS` as paths.
    ///
    /// Without this, `shape_paths` returning only top-level keys would
    /// leave the test above passing against a `META_KEYS` that had been
    /// flattened back — the failure mode this fix exists to close.
    #[test]
    fn the_shape_walk_reaches_nested_and_array_fields() {
        let v = serde_json::json!({
            "scalar": 1,
            "nothing": serde_json::Value::Null,
            "nested": { "deep": { "leaf": "x" } },
            "objects": [{ "kind": "a" }, { "kind": "b" }],
            "scalars": ["a", "b"],
            "empty": [],
        });
        assert_eq!(
            shape_paths(&v),
            vec![
                "empty[]",
                "nested.deep.leaf",
                "nothing",
                "objects[].kind",
                "scalar",
                "scalars[]",
            ]
        );
    }

    /// And the marker is load-bearing: two panels identical in commit
    /// and topics but built by servers of different shapes must not
    /// share a tag.
    ///
    /// Asserted against a *recorded* tag rather than by mutating the
    /// constant, because the constant is compile-time. If this literal
    /// ever has to be updated, that is the test telling you every
    /// cached panel in the world just invalidated — which is correct,
    /// and worth being told about rather than discovering in a graph.
    #[test]
    fn the_shape_marker_actually_reaches_the_tag() {
        let with_shape = meta_etag("c0ffee", &["git".into()]);
        // The same two inputs, hashed the way the tag was computed
        // before the marker existed.
        let without = {
            let payload = "c0ffee\ngit";
            format!(
                "\"{}\"",
                hex(&objwrite::hash_object(OBJ_BLOB, payload.as_bytes()))
            )
        };
        assert_ne!(
            with_shape, without,
            "the shape marker is not reaching the ETag: this tag is \
             byte-for-byte what the old, broken computation produced, \
             so every stale cached panel would still be answered 304"
        );
    }

    use super::*;

    /// Every mark in the table survives its own normalisation.
    ///
    /// This is the test that keeps the table honest as it grows. A mark
    /// written with a comma, an apostrophe or a double space in it can
    /// never match a normalised document — it would simply never fire,
    /// which looks exactly like "that licence is rare" and would be
    /// found by nobody.
    #[test]
    fn every_fingerprint_mark_is_already_normalised() {
        for f in LICENSES {
            for m in f.marks.iter().chain(f.absent.iter()) {
                assert_eq!(
                    &normalize_license(m),
                    m,
                    "{}: mark {m:?} is not in normalised form and can never match",
                    f.spdx
                );
            }
        }
    }

    /// No two entries carry the same SPDX id, and none has an empty mark
    /// list — an entry with no marks matches everything and would make
    /// every licence in the world ambiguous.
    #[test]
    fn the_table_is_well_formed() {
        let mut seen: Vec<&str> = Vec::new();
        for f in LICENSES {
            assert!(!f.marks.is_empty(), "{} has no marks", f.spdx);
            assert!(!seen.contains(&f.spdx), "{} appears twice", f.spdx);
            seen.push(f.spdx);
        }
    }

    /// Normalisation erases everything that legitimately varies between
    /// two copies of one licence and nothing else.
    #[test]
    fn normalisation_erases_only_what_varies() {
        assert_eq!(
            normalize_license("Copyright (C) 2026  Ada\r\nLovelace!"),
            "copyright c 2026 ada lovelace"
        );
        // Wrapping is not meaning: the same sentence wrapped at 40 and
        // at 80 columns normalises identically.
        assert_eq!(
            normalize_license("and/or distribute\nthis software"),
            normalize_license("and/or distribute this software")
        );
        // Curly quotes, straight quotes and no quotes are one string.
        assert_eq!(
            normalize_license("provided \u{201c}as is\u{201d}"),
            normalize_license("provided \"as is\"")
        );
        assert_eq!(normalize_license("   "), "");
    }

    fn spdx(text: &str) -> Option<&'static str> {
        detect_license(text.as_bytes()).map(|f| f.spdx)
    }

    #[test]
    fn the_canonical_texts_identify_themselves() {
        assert_eq!(spdx(MIT), Some("MIT"));
        assert_eq!(spdx(APACHE), Some("Apache-2.0"));
        assert_eq!(spdx(BSD3), Some("BSD-3-Clause"));
        assert_eq!(spdx(BSD2), Some("BSD-2-Clause"));
        assert_eq!(spdx(ISC), Some("ISC"));
        assert_eq!(spdx(GPL3), Some("GPL-3.0"));
        assert_eq!(spdx(GPL2), Some("GPL-2.0"));
        assert_eq!(spdx(LGPL3), Some("LGPL-3.0"));
        assert_eq!(spdx(AGPL3), Some("AGPL-3.0"));
        assert_eq!(spdx(MPL2), Some("MPL-2.0"));
        assert_eq!(spdx(UNLICENSE), Some("Unlicense"));
        assert_eq!(spdx(CC0), Some("CC0-1.0"));
    }

    /// The families that quote each other must not be confused, and
    /// this is the pair of cases the `absent` marks exist for.
    ///
    /// LGPL-3.0 contains the GPL-3.0 title and version line verbatim —
    /// it says so in its own second paragraph — and BSD-3-Clause is
    /// BSD-2-Clause with one more clause. Without the exclusions both
    /// pairs would fingerprint as two licences and collapse to
    /// `unrecognised`, which is safe but useless.
    #[test]
    fn a_licence_that_quotes_another_is_still_itself() {
        assert_eq!(spdx(LGPL3), Some("LGPL-3.0"));
        assert_eq!(spdx(AGPL3), Some("AGPL-3.0"));
        assert_eq!(spdx(BSD3), Some("BSD-3-Clause"));
        // And the reverse direction: a plain GPL-3.0 is not read as the
        // lesser one.
        assert_eq!(spdx(GPL3), Some("GPL-3.0"));
    }

    /// **The rule the whole feature rests on.** Anything the table is
    /// not certain about is unrecognised, and never the nearest guess.
    #[test]
    fn anything_uncertain_is_unrecognised_rather_than_guessed() {
        // A README that happens to be called LICENSE.
        assert_eq!(spdx("# widget\n\nA fast thing.\n"), None);
        // Empty, and whitespace.
        assert_eq!(spdx(""), None);
        assert_eq!(spdx("\n\n   \n"), None);
        // A licence *name* with none of the text. This is the case a
        // naive implementation gets wrong, and it is the common one:
        // plenty of repositories ship a one-line LICENSE saying "MIT".
        assert_eq!(spdx("MIT License\n\nCopyright (c) 2026 Ada\n"), None);
        assert_eq!(spdx("Licensed under the Apache License.\n"), None);
        // Half of MIT: the grant without the warranty disclaimer.
        assert_eq!(
            spdx("Permission is hereby granted, free of charge, to any person obtaining a copy of this software.\n"),
            None
        );
        // An Apache NOTICE rather than the licence.
        assert_eq!(
            spdx("widget\nCopyright 2026 Ada\n\nThis product includes software developed at Ada Inc.\n"),
            None
        );
        // Not UTF-8 at all.
        assert_eq!(
            detect_license(&[0xff, 0xfe, 0x00, 0x41]).map(|f| f.spdx),
            None
        );
    }

    /// Two licences in one file is a decision a reader has to make, so
    /// it is one we decline. A dual-licensed project is real and common,
    /// and reporting the first hit would name whichever one this table
    /// happens to list first.
    #[test]
    fn a_file_holding_two_licences_is_unrecognised() {
        let both = format!("{MIT}\n\n---\n\n{APACHE}");
        assert_eq!(spdx(&both), None);
        // Neither ordering wins, which is what makes the answer
        // independent of the table's order.
        let other = format!("{APACHE}\n\n---\n\n{MIT}");
        assert_eq!(spdx(&other), None);
    }

    /// Bounded, and the bound is a refusal (I13). A licence identified
    /// from a fragment of a 40 MB file is a licence identified from a
    /// fragment somebody chose.
    #[test]
    fn an_oversized_licence_file_is_refused_rather_than_truncated() {
        let mut big = String::from(MIT);
        assert_eq!(spdx(&big), Some("MIT"));
        while big.len() <= LICENSE_BYTES {
            big.push('x');
        }
        assert_eq!(
            spdx(&big),
            None,
            "the bound was truncated past, not refused"
        );
    }

    /// Every shape a licence file is actually written in.
    ///
    /// The decorated names are the ones this got wrong: the first
    /// version recognised only `LICENSE`/`LICENCE`/`COPYING`, so
    /// `rust-lang/rust` — `LICENSE-APACHE` beside `LICENSE-MIT`, the
    /// convention across the whole Rust ecosystem — matched nothing and
    /// rendered no licence row at all. A dual-licensed project reading
    /// as unlicensed is worse than the guess the narrowness was
    /// avoiding, and only opening the real page showed it.
    #[test]
    fn every_shape_a_licence_file_is_written_in_is_recognised() {
        for yes in [
            "LICENSE",
            "license",
            "LICENSE.md",
            "LICENSE.txt",
            "LICENCE",
            "Licence.rst",
            "COPYING",
            "copying.txt",
            "LICENSE-MIT",
            "LICENSE-APACHE",
            "license-apache-2.0.txt",
            "COPYING.LESSER",
            "LICENCE_MIT",
        ] {
            assert!(is_license_name(yes), "{yes}");
        }
        // Breadth stops at things that are not licence files. It is
        // safe to be broad here only because this decides *how many*
        // there are; naming one still needs exactly one.
        for no in [
            "LICENSES",
            "license.html",
            "LICENSE.rs",
            "licensed.md",
            "licensing-faq.md",
            "README.md",
            "",
            ".license",
        ] {
            assert!(!is_license_name(no), "{no}");
        }
    }

    #[test]
    fn community_files_are_recognised_in_both_spellings() {
        assert_eq!(community_kind("CONTRIBUTING.md"), Some("contributing"));
        assert_eq!(community_kind("contributing"), Some("contributing"));
        assert_eq!(
            community_kind("CODE_OF_CONDUCT.md"),
            Some("code_of_conduct")
        );
        assert_eq!(
            community_kind("code-of-conduct.rst"),
            Some("code_of_conduct")
        );
        assert_eq!(community_kind("SECURITY.md"), Some("security"));
        assert_eq!(community_kind("security.txt"), Some("security"));
        for no in [
            "README.md",
            "CONTRIBUTORS.md",
            "SECURITY.yml",
            "",
            "code.md",
        ] {
            assert_eq!(community_kind(no), None, "{no}");
        }
    }

    /// A README is recognised in every shape the naming rule covers,
    /// and in no other.
    ///
    /// The boundary is deliberate and it is the file's, not this
    /// function's: [`meta_stem`] strips exactly `.md`, `.txt` and
    /// `.rst`, which is the same set `community_kind` and
    /// `is_license_name` use. `README.adoc` and `README.org` are
    /// therefore *not* recognised, and that is pinned here rather than
    /// quietly widened — the day asciidoc is worth supporting, it is
    /// worth supporting for a CONTRIBUTING guide too, and the rule
    /// should move in one place for all three.
    #[test]
    fn a_readme_is_recognised_in_the_shapes_it_is_written_in() {
        for yes in [
            "README.md",
            "readme.md",
            "ReadMe.MD",
            "README",
            "readme",
            "README.txt",
            "README.rst",
        ] {
            assert!(is_readme_name(yes), "{yes}");
        }
        for no in [
            // Not a README, however much of the word it contains.
            "READMENOW.md",
            "readme-first.md",
            "read-me.md",
            "CONTRIBUTING.md",
            "",
            // A dotfile, not a README with an extension stripped.
            ".readme",
            // The extensions the file's rule does not cover.
            "README.adoc",
            "README.org",
            "README.html",
        ] {
            assert!(!is_readme_name(no), "{no}");
        }
        // And the two rules stay disjoint: a README is never reported
        // as a community `kind`, because it is its own field.
        assert_eq!(community_kind("README.md"), None);
        assert!(!is_license_name("README.md"));
    }

    #[test]
    fn a_language_is_an_extension_and_a_dotfile_has_none() {
        assert_eq!(language_of("main.rs"), Some("Rust"));
        assert_eq!(language_of("App.TSX"), Some("TypeScript"));
        assert_eq!(language_of("a/b/c.py"), Some("Python"));
        // Data and prose are not languages: counting them turns every
        // documented project into a Markdown project.
        for no in [
            "README.md",
            "Cargo.toml",
            "data.json",
            "notes.txt",
            "x.yaml",
        ] {
            assert_eq!(language_of(no), None, "{no}");
        }
        // A dotfile's suffix is not an extension. `.rs` as a filename is
        // a config file somebody named oddly, not Rust source.
        assert_eq!(language_of(".rs"), None);
        assert_eq!(language_of(".gitignore"), None);
        // No dot at all.
        assert_eq!(language_of("Makefile"), None);
        assert_eq!(language_of(""), None);
    }

    /// Ranking is by bytes, and ties break on the name.
    ///
    /// The tie-break is not tidiness: the client assigns colours by
    /// rank, so an unstable order means a repository with two equal
    /// languages swaps its bar's colours on every request.
    #[test]
    fn ranking_is_by_bytes_and_ties_break_on_the_name() {
        let mut counts = std::collections::HashMap::new();
        counts.insert("Rust".to_string(), 100u64);
        counts.insert("Zig".to_string(), 50);
        counts.insert("Go".to_string(), 50);
        counts.insert("C".to_string(), 10);
        let out = rank(&counts);
        assert_eq!(
            out.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(),
            ["Rust", "Go", "Zig", "C"]
        );
        assert_eq!(out[0].bytes, 100);
        assert!(rank(&std::collections::HashMap::new()).is_empty());
    }

    #[test]
    fn the_language_list_is_bounded() {
        let counts: std::collections::HashMap<String, u64> = (0..MAX_LANGUAGES * 3)
            .map(|i| (format!("L{i:03}"), (1000 - i) as u64))
            .collect();
        assert_eq!(rank(&counts).len(), MAX_LANGUAGES);
    }

    // -----------------------------------------------------------------
    // Fixtures.
    //
    // Excerpts of the canonical SPDX texts — enough of each to carry
    // the marks and the surrounding sentences, not the whole document.
    // Their honest status is stated in the module report: a positive
    // test written from the same reading as the fingerprint is weak
    // evidence, and the load-bearing tests above are the negative ones —
    // near-misses, name-only files, ambiguity, and the bound.
    // -----------------------------------------------------------------

    const MIT: &str = "MIT License\n\nCopyright (c) 2026 Ada Lovelace\n\n\
Permission is hereby granted, free of charge, to any person obtaining a copy \
of this software and associated documentation files (the \"Software\"), to deal \
in the Software without restriction, including without limitation the rights \
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell \
copies of the Software, and to permit persons to whom the Software is \
furnished to do so, subject to the following conditions:\n\n\
The above copyright notice and this permission notice shall be included in all \
copies or substantial portions of the Software.\n\n\
THE SOFTWARE IS PROVIDED \"AS IS\", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR \
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, \
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.\n";

    const APACHE: &str = "                                 Apache License\n\
                           Version 2.0, January 2004\n\
                        http://www.apache.org/licenses/\n\n\
   TERMS AND CONDITIONS FOR USE, REPRODUCTION, AND DISTRIBUTION\n\n\
   1. Definitions.\n\n\
      \"License\" shall mean the terms and conditions for use, reproduction,\n\
      and distribution as defined by Sections 1 through 9 of this document.\n";

    const BSD3: &str = "BSD 3-Clause License\n\nCopyright (c) 2026, Ada Lovelace\n\n\
Redistribution and use in source and binary forms, with or without \
modification, are permitted provided that the following conditions are met:\n\n\
1. Redistributions of source code must retain the above copyright notice, this \
list of conditions and the following disclaimer.\n\n\
2. Redistributions in binary form must reproduce the above copyright notice, \
this list of conditions and the following disclaimer in the documentation \
and/or other materials provided with the distribution.\n\n\
3. Neither the name of the copyright holder nor the names of its contributors \
may be used to endorse or promote products derived from this software without \
specific prior written permission.\n";

    const BSD2: &str = "BSD 2-Clause License\n\nCopyright (c) 2026, Ada Lovelace\n\n\
Redistribution and use in source and binary forms, with or without \
modification, are permitted provided that the following conditions are met:\n\n\
1. Redistributions of source code must retain the above copyright notice, this \
list of conditions and the following disclaimer.\n\n\
2. Redistributions in binary form must reproduce the above copyright notice, \
this list of conditions and the following disclaimer in the documentation \
and/or other materials provided with the distribution.\n";

    const ISC: &str = "ISC License\n\nCopyright (c) 2026 Ada Lovelace\n\n\
Permission to use, copy, modify, and/or distribute this software for any \
purpose with or without fee is hereby granted, provided that the above \
copyright notice and this permission notice appear in all copies.\n";

    const GPL3: &str = "                    GNU GENERAL PUBLIC LICENSE\n\
                       Version 3, 29 June 2007\n\n\
 Copyright (C) 2007 Free Software Foundation, Inc. <https://fsf.org/>\n\
 Everyone is permitted to copy and distribute verbatim copies\n\
 of this license document, but changing it is not allowed.\n";

    const GPL2: &str = "                    GNU GENERAL PUBLIC LICENSE\n\
                       Version 2, June 1991\n\n\
 Copyright (C) 1989, 1991 Free Software Foundation, Inc.\n\
 Everyone is permitted to copy and distribute verbatim copies\n\
 of this license document, but changing it is not allowed.\n";

    const LGPL3: &str = "                   GNU LESSER GENERAL PUBLIC LICENSE\n\
                       Version 3, 29 June 2007\n\n\
 Copyright (C) 2007 Free Software Foundation, Inc. <https://fsf.org/>\n\n\
  This version of the GNU Lesser General Public License incorporates\n\
the terms and conditions of version 3 of the GNU General Public\n\
License, supplemented by the additional permissions listed below.\n";

    const AGPL3: &str = "                    GNU AFFERO GENERAL PUBLIC LICENSE\n\
                       Version 3, 19 November 2007\n\n\
 Copyright (C) 2007 Free Software Foundation, Inc. <https://fsf.org/>\n\
 Everyone is permitted to copy and distribute verbatim copies\n\
 of this license document, but changing it is not allowed.\n";

    const MPL2: &str = "Mozilla Public License Version 2.0\n\
==================================\n\n\
1. Definitions\n\
--------------\n\n\
1.1. \"Contributor\"\n\
    means each individual or legal entity that creates, contributes to\n\
    the creation of, or owns Covered Software.\n";

    const UNLICENSE: &str =
        "This is free and unencumbered software released into the public domain.\n\n\
Anyone is free to copy, modify, publish, use, compile, sell, or distribute this \
software, either in source code form or as a compiled binary, for any purpose, \
commercial or non-commercial, and by any means.\n";

    const CC0: &str = "Creative Commons Legal Code\n\nCC0 1.0 Universal\n\n\
    CREATIVE COMMONS CORPORATION IS NOT A LAW FIRM AND DOES NOT PROVIDE\n\
    LEGAL SERVICES.\n";
}
