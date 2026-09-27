//! Error classification: the one place that decides what a stringly-typed
//! error *means*.
//!
//! Two layers below the API still answer with `String` — `stratum-store`
//! and `stratum-proto` are vendored and keep the research repo's error
//! type — so a caller that has to tell "the object is absent" from "the
//! store is broken", or "that name is taken" from "the database fell
//! over", has only the message to go on. That is a real constraint. What
//! is not forced is the way it used to be done: `e.contains("HTTP 404")`,
//! `e.contains("already exists")`, `e.contains("invalid")`, written out
//! by hand at fifteen call sites.
//!
//! Why that is dangerous is written down in CLAUDE.md as the repo's own
//! worked example: a classifier matching the bare substring `401` read a
//! repository named `rfc-403` as "this looks private", and sent people to
//! a flow that could not help them. The needle was not in our text at
//! all — it was in the user's.
//!
//! So every predicate here is **anchored**, and the anchor is derived from
//! the phrasing the producing code actually emits:
//!
//! - Object-store status errors are formatted `<VERB> <key>: HTTP <code>`
//!   by `stratum_store::ObjectStore`, and callers only ever *prefix* that
//!   (`read current manifest: …`, `task join: …`). So the status is the
//!   tail of the message, and [`store_status`] reads it there rather than
//!   hunting for `HTTP 404` anywhere in the line.
//! - Control-plane and engine phrases are always in a format string of
//!   ours, and every value interpolated beside one goes in as `{x:?}` —
//!   Rust debug, which quotes it and escapes the quotes inside it. So
//!   user text always lands inside a quoted span, and [`contains_unquoted`]
//!   only counts a phrase that occurs outside every such span.
//!
//! Being wrong in one place is recoverable. Being wrong in fifteen is how
//! the `rfc-403` bug survived.

/// The HTTP status an object-store error carries, if it carries one.
///
/// `stratum_store` formats every status failure as `<VERB> <key>: HTTP
/// <code>`, so the code is the last thing in the message. Requiring
/// exactly three digits at the very end is what stops a key, ref, path or
/// repository name that happens to contain the bytes `HTTP 404` from
/// being read as a status — and it also refuses store errors that merely
/// *mention* a number, like `expected 206 for range, got 404`, which is a
/// misbehaving store rather than an absent object.
pub fn store_status(err: &str) -> Option<u16> {
    let code = err.rsplit_once(": HTTP ")?.1;
    if code.len() == 3 && code.bytes().all(|b| b.is_ascii_digit()) {
        code.parse().ok()
    } else {
        None
    }
}

/// The object is not there: the store answered 404.
///
/// Deliberately *only* 404. Real S3 answers **403 AccessDenied** for a
/// missing key when the principal lacks `s3:ListBucket`, and it is
/// tempting to accept that here so a least-privilege deployment stops
/// failing its first push. It would be a serious mistake: every caller of
/// this reads "absent" as "empty repository, carry on", so a genuine
/// permission failure — or a bucket policy that changed under a live
/// fleet — would be read as "this repo has no history" and a push would
/// be verified against nothing. The backend has to answer 404, which is
/// exactly what `stratum_testkit::contract`'s
/// `a_missing_key_reports_404_not_403` case exists to prove. When it does
/// not, [`is_access_denied`] and [`diagnose_store`] make the failure say
/// so instead of leaving an operator to guess.
pub fn is_absent(err: &str) -> bool {
    store_status(err) == Some(404)
}

/// The store refused the request: 403. Not absence — see [`is_absent`].
pub fn is_access_denied(err: &str) -> bool {
    store_status(err) == Some(403)
}

/// Name the most likely cause of a store 403 in the error itself.
///
/// A bare `GET o/…/manifest.json: HTTP 403` bubbling up as a 500 tells an
/// operator nothing, and this failure has one overwhelmingly common
/// cause: an IAM policy that grants `s3:GetObject` but not
/// `s3:ListBucket`, under which S3 answers 403 for keys that simply are
/// not there. Everything else passes through untouched.
pub fn diagnose_store(err: String) -> String {
    if is_access_denied(&err) {
        format!(
            "{err} — the store answered 403 where an absent key must answer \
             404; the usual cause is an IAM policy without s3:ListBucket \
             (stratum_testkit::contract::a_missing_key_reports_404_not_403)"
        )
    } else {
        err
    }
}

/// A control-plane uniqueness violation: this name/address is taken.
///
/// Anchored on `stratum_control`'s five emitters — `org {name:?} already
/// exists`, `organization …`, `repo …`, `team …`, and `a user with that
/// email already exists`. Every one of them interpolates the caller's
/// name with `{:?}`, so a repository, org or team called anything at all
/// lands inside quotes and cannot fabricate the phrase. A Postgres error
/// that happens to quote an identifier — `relation "already exists" does
/// not exist` — is likewise not one of these.
pub fn is_already_exists(err: &str) -> bool {
    contains_unquoted(err, "already exists")
}

/// A control-plane validation refusal: the input was malformed.
///
/// Every emitter (`invalid {noun} name …`, `invalid repo name …`,
/// `invalid description: …`, `invalid email address`, `invalid branch …`,
/// `invalid team name …`, `invalid query: …`, `invalid change key …`,
/// `invalid check name …`) *starts* with the word, with the offending
/// value after it. So a repository named `my-invalid-name` no longer
/// turns a "that name is taken" into a 400 — and neither does a database
/// error whose text mentions `invalid input syntax`, which is a 500 and
/// not the caller's fault.
pub fn is_invalid_input(err: &str) -> bool {
    err.starts_with("invalid ")
}

/// The revision the caller named does not resolve.
pub fn is_unknown_rev(err: &str) -> bool {
    contains_unquoted(err, "unknown rev ")
}

/// The path is not present in the layout at that revision.
///
/// Both emitters — `{oid}: not in this layout` and `{path:?} not in this
/// layout at {at:?}` — say it outside the quoted value, so a file
/// genuinely named `not in this layout` classifies on its real outcome
/// rather than on its name.
pub fn is_missing_path(err: &str) -> bool {
    contains_unquoted(err, "not in this layout")
}

/// The path resolved, but to the other kind of object — a tree asked for
/// as a file, or a file asked for as a directory. A 404 to the browser
/// that followed the link, not a 500.
pub fn is_wrong_object_kind(err: &str) -> bool {
    contains_unquoted(err, "is not a tree") || contains_unquoted(err, "is not a blob")
}

/// Does `needle` occur in `hay` outside every debug-quoted (`"…"`) span?
///
/// Rust's `{x:?}` on a string wraps it in `"` and escapes any `"` and `\`
/// inside, so text that arrived from a user is always *within* a quoted
/// span and can never close one early. A phrase found outside every span
/// is therefore one our own format string put there. This is the general
/// form of the fix CLAUDE.md describes: match what our code said, not
/// what the user's data happens to spell.
fn contains_unquoted(hay: &str, needle: &str) -> bool {
    let mut quoted = false;
    let mut escaped = false;
    for (i, c) in hay.char_indices() {
        if quoted {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                quoted = false;
            }
        } else if c == '"' {
            quoted = true;
        } else if hay[i..].starts_with(needle) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact bytes `stratum_store::ObjectStore` produces, and the
    /// exact wrappers its callers put in front of them.
    #[test]
    fn store_statuses_are_read_from_the_tail() {
        assert_eq!(
            store_status("GET o/01H/r/01J/L1/manifest.json: HTTP 404"),
            Some(404)
        );
        assert_eq!(store_status("LIST o/01H/r/01J: HTTP 403"), Some(403));
        assert_eq!(store_status("DELETE o/01H/r/01J/x: HTTP 500"), Some(500));
        // The wrappers real call sites add are all prefixes.
        assert_eq!(
            store_status("read current manifest: GET o/1/r/2/L1/manifest.json: HTTP 404"),
            Some(404)
        );
        assert_eq!(
            store_status("task join: GET o/1/r/2/packs/p.pack: HTTP 404"),
            Some(404)
        );
        assert!(is_absent("GET o/1/r/2/L1/locator.hdr: HTTP 404"));
        assert!(!is_absent("GET o/1/r/2/L1/locator.hdr: HTTP 500"));
    }

    /// The `rfc-403` class: the needle is in the user's data, not ours.
    /// A key naming a repository called `HTTP 404` is not a 404, and a
    /// store that ignored `Range` is not an absent object.
    #[test]
    fn a_status_in_the_middle_of_a_message_is_not_a_status() {
        assert_eq!(store_status("GET o/1/r/HTTP 404/L1/manifest.json"), None);
        assert!(!is_absent("GET mirror/HTTP 404.pack: read timed out"));
        assert!(!is_absent(
            "GET o/1/r/2/seg-0: expected 206 for range, got 404"
        ));
        // Not three digits, or not at the end.
        assert_eq!(store_status("GET x: HTTP 4040"), None);
        assert_eq!(store_status("GET x: HTTP 40"), None);
        assert_eq!(store_status("GET x: HTTP 404 (retrying)"), None);
        assert_eq!(store_status("nothing here at all"), None);
    }

    /// 403 is refused as absence on purpose, and says why out loud.
    #[test]
    fn a_403_is_denial_not_absence() {
        let denied = "GET o/1/r/2/L1/manifest.json: HTTP 403";
        assert!(!is_absent(denied));
        assert!(is_access_denied(denied));
        let explained = diagnose_store(denied.to_string());
        assert!(explained.starts_with(denied), "{explained}");
        assert!(explained.contains("s3:ListBucket"), "{explained}");
        // Everything else passes through byte-for-byte.
        let other = "GET o/1/r/2/L1/manifest.json: HTTP 500".to_string();
        assert_eq!(diagnose_store(other.clone()), other);
        assert!(!is_access_denied("GET o/1/r/HTTP 403/x: read timed out"));
    }

    /// The five `already exists` emitters in `stratum_control`.
    #[test]
    fn uniqueness_violations_are_recognised() {
        assert!(is_already_exists(r#"org "acme" already exists"#));
        assert!(is_already_exists(r#"organization "acme" already exists"#));
        assert!(is_already_exists(r#"repo "kernel" already exists"#));
        assert!(is_already_exists(r#"team "reviewers" already exists"#));
        assert!(is_already_exists("a user with that email already exists"));
    }

    /// The old code answered 409 for anything whose text contained the
    /// phrase. A Postgres failure that quotes an identifier is a 500.
    #[test]
    fn a_quoted_mention_is_not_a_uniqueness_violation() {
        assert!(!is_already_exists(
            r#"db error: ERROR: relation "already exists" does not exist"#
        ));
        assert!(!is_already_exists(
            r#"invalid repo name "already exists-ish""#
        ));
        assert!(!is_already_exists("repo vanished after insert"));
    }

    /// `invalid` is a prefix in every emitter, never a floating word.
    #[test]
    fn validation_refusals_are_recognised() {
        assert!(is_invalid_input(r#"invalid repo name "Nope!""#));
        assert!(is_invalid_input(r#"invalid org name "..""#));
        assert!(is_invalid_input(
            "invalid description: no control characters"
        ));
        assert!(is_invalid_input("invalid email address"));
        assert!(is_invalid_input("invalid query: at most 200 characters"));
    }

    /// The negative that matters: a *name* containing "invalid" used to
    /// be enough to turn its error into a 400.
    #[test]
    fn the_word_invalid_inside_a_name_is_not_a_validation_refusal() {
        assert!(!is_invalid_input(r#"repo "invalid-input" already exists"#));
        assert!(!is_invalid_input(
            r#"org "my-invalid-org" name "my-invalid-org" is reserved"#
        ));
        // A database failure is not the caller's bad request.
        assert!(!is_invalid_input(
            "db error: ERROR: invalid input syntax for type bigint"
        ));
        assert!(!is_invalid_input(
            r#"storage init failed: PUT o/1/r/2: invalid something"#
        ));
    }

    /// Revision / path / kind phrases, and the same needle hidden in a
    /// user-supplied path or ref name.
    #[test]
    fn layout_lookup_failures_are_recognised() {
        assert!(is_unknown_rev(r#"unknown rev "no-such-branch""#));
        assert!(is_missing_path("deadbeef: not in this layout"));
        assert!(is_missing_path(
            r#""src/main.rs" not in this layout at "main""#
        ));
        assert!(is_wrong_object_kind(r#""docs" is not a blob"#));
        assert!(is_wrong_object_kind(r#""README.md" is not a tree"#));

        assert!(!is_unknown_rev(
            r#""unknown rev.txt" not in this layout at "main""#
        ));
        assert!(!is_missing_path(r#"unknown rev "not in this layout""#));
        assert!(!is_wrong_object_kind(
            r#""this file is not a tree.md" not in this layout at "main""#
        ));
    }

    /// The quote scanner itself, including the escapes `{:?}` emits.
    #[test]
    fn escaped_quotes_do_not_end_a_quoted_span() {
        // A path containing a literal quote debug-prints as \" — the span
        // must survive it, or everything after would count as ours.
        assert!(!contains_unquoted(r#""a\"needle\"b" is fine"#, "needle"));
        // A trailing backslash inside the span likewise.
        assert!(!contains_unquoted(r#""a\\\"needle" x"#, "needle"));
        assert!(contains_unquoted("needle at the very start", "needle"));
        assert!(contains_unquoted(r#""quoted" then needle"#, "needle"));
        assert!(!contains_unquoted("", "needle"));
        // Multi-byte input must not panic on the slice boundaries.
        assert!(contains_unquoted("héllo needle", "needle"));
        assert!(!contains_unquoted(r#""héllo needle""#, "needle"));
    }
}
