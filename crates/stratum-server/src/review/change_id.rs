//! Change-Id extraction: the trailer that gives a commit a review
//! identity that survives rebase and amend.
//!
//! `Change-Id: I<hex>` in the commit message's last paragraph, the way
//! Gerrit's commit-msg hook writes it. A commit without one still gets a
//! change — keyed from the commit oid — but that identity dies with the
//! oid: amend or rebase mints a new change. The docs say this out loud;
//! the server cannot inject a trailer into an immutable pushed commit.

/// Every value of trailer `key` in the message's trailer block — the
/// last non-empty paragraph — in the order they were written.
///
/// One grammar, several trailers. `Change-Id` was the first, and the
/// contribution walker needs `Co-authored-by`; a second scanner written
/// next door would drift from this one on the first edge case somebody
/// fixed in only one of them.
///
/// **All of them, not the nearest one.** `Co-authored-by` legitimately
/// repeats — that is its entire purpose, and a commit may carry four
/// co-authors. Keeping only the last would make "is any co-author an
/// agent?" depend on the order somebody happened to write the lines in.
/// [`trailer`] is the single-value form, for `Change-Id`, which really
/// does want the nearest match.
///
/// **Keys match ASCII-case-insensitively**, and that is load-bearing
/// rather than tidy. Git treats trailer keys case-insensitively, and
/// GitHub's own UI writes `Co-authored-by:` — lowercase `a`, lowercase
/// `b`. An exact match would find none of those, so authorship
/// extraction over a decade of imported history would quietly find
/// nothing at all: no error, no failing test, just an empty graph for
/// exactly the people the feature exists to win. Do not tidy this back
/// into `strip_prefix(key)`.
///
/// Values are returned *unvalidated*: what a valid one looks like is
/// the caller's question.
pub fn trailers<'a>(message: &'a str, key: &str) -> Vec<&'a str> {
    let Some(block) = message.rsplit("\n\n").find(|p| !p.trim().is_empty()) else {
        return Vec::new();
    };
    block
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            // `get` rather than slicing: a key length that lands inside a
            // multi-byte character answers `None` instead of panicking,
            // and a commit message is arbitrary bytes from a stranger.
            if !line.get(..key.len())?.eq_ignore_ascii_case(key) {
                return None;
            }
            line.get(key.len()..)?.strip_prefix(':').map(str::trim)
        })
        .collect()
}

/// The nearest value of trailer `key` — the bottom-most, the way git
/// reads a trailer block when it wants one answer.
///
/// A malformed value there is returned as it stands rather than letting
/// an older line further up stand in for it: silently reaching past a
/// bad line is how a stale identity outlives the edit that replaced it.
pub fn trailer<'a>(message: &'a str, key: &str) -> Option<&'a str> {
    trailers(message, key).pop()
}

/// The trailer's key, normalized to `I` + lowercase hex.
pub fn parse_change_id(message: &str) -> Option<String> {
    let hex = trailer(message, "Change-Id")?.strip_prefix(['I', 'i'])?;
    if (8..=40).contains(&hex.len()) && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(format!("I{}", hex.to_lowercase()));
    }
    None
}

/// The key for a commit with no trailer: `g` + the commit oid. Stable
/// for the exact commit, gone the moment the commit is rewritten.
pub fn derive_key(commit_oid: &str) -> String {
    format!("g{commit_oid}")
}

/// The key for a commit: its trailer if it carries a valid one, else
/// derived from the oid.
pub fn key_for(message: &str, commit_oid: &str) -> String {
    parse_change_id(message).unwrap_or_else(|| derive_key(commit_oid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trailer_in_the_last_paragraph_is_the_key() {
        let msg = "fix the gateway\n\nlonger prose here\n\nChange-Id: Iabc12345\n";
        assert_eq!(parse_change_id(msg), Some("Iabc12345".into()));
        // Normalized to lowercase hex behind the I.
        let msg = "t\n\nChange-Id: IABCDEF99\n";
        assert_eq!(parse_change_id(msg), Some("Iabcdef99".into()));
        // Other trailers around it are fine.
        let msg = "t\n\nSigned-off-by: A <a@b.c>\nChange-Id: Ideadbeef\nBug: 42\n";
        assert_eq!(parse_change_id(msg), Some("Ideadbeef".into()));
    }

    #[test]
    fn a_change_id_outside_the_last_paragraph_does_not_count() {
        // Prose mentioning a Change-Id is not a trailer.
        let msg =
            "revert \"x\"\n\nThis reverts Change-Id: Iaaaa1111 from last week.\n\nno trailers";
        assert_eq!(parse_change_id(msg), None);
        // A trailer in the first paragraph of a two-paragraph message is
        // not the trailer block.
        let msg = "Change-Id: Ibbbb2222\n\nactual last paragraph";
        assert_eq!(parse_change_id(msg), None);
    }

    #[test]
    fn malformed_trailers_are_ignored_not_guessed_at() {
        for msg in [
            "t\n\nChange-Id: abc123456\n",  // no I prefix
            "t\n\nChange-Id: I12345\n",     // too short
            "t\n\nChange-Id: Ixyzxyzxyz\n", // not hex
            "t\n\nChange-Id: I\n",          // empty
            "t\n\nChange-Id:\n",            // nothing
            "t",                            // no trailer block at all
        ] {
            assert_eq!(parse_change_id(msg), None, "{msg:?}");
        }
        let long = format!("t\n\nChange-Id: I{}\n", "a".repeat(41));
        assert_eq!(parse_change_id(&long), None);
    }

    #[test]
    fn a_key_matches_however_it_is_capitalised() {
        // The three spellings that actually occur in the wild, side by
        // side. GitHub's UI writes the middle one, so an exact-match
        // parser finds nothing in imported history — and finds it
        // silently.
        for msg in [
            "t\n\nCo-authored-by: A <a@b.c>\n",
            "t\n\nCo-Authored-By: A <a@b.c>\n",
            "t\n\nco-authored-by: A <a@b.c>\n",
            "t\n\nCO-AUTHORED-BY: A <a@b.c>\n",
        ] {
            assert_eq!(
                trailers(msg, "Co-authored-by"),
                vec!["A <a@b.c>"],
                "{msg:?}"
            );
        }
        // The key the caller asks with is just as free.
        assert_eq!(
            trailer("t\n\nCo-authored-by: A <a@b.c>\n", "CO-AUTHORED-BY"),
            Some("A <a@b.c>")
        );
        // Change-Id too: Gerrit's hook writes one spelling, and a human
        // typing another is not wrong. Two parsers must not disagree
        // about what a trailer is.
        assert_eq!(
            parse_change_id("t\n\nchange-id: Iabc12345\n"),
            Some("Iabc12345".into())
        );
        // A near-miss is still a miss.
        assert!(trailers("t\n\nCo-authored-byx: A <a@b.c>\n", "Co-authored-by").is_empty());
        assert!(trailers("t\n\nXCo-authored-by: A <a@b.c>\n", "Co-authored-by").is_empty());
    }

    #[test]
    fn every_repeat_is_returned_in_the_order_written() {
        // A commit may carry four co-authors; that is the trailer's
        // whole purpose. Keeping only one would make "is any co-author
        // an agent?" depend on the order the lines happen to be in.
        let msg = "mob session\n\nCo-authored-by: A <a@x.test>\n\
                   Co-authored-by: B <b@x.test>\n\
                   Co-Authored-By: C <c@x.test>\n";
        assert_eq!(
            trailers(msg, "Co-authored-by"),
            vec!["A <a@x.test>", "B <b@x.test>", "C <c@x.test>"]
        );
        // The single-value form keeps the nearest — the bottom-most.
        assert_eq!(trailer(msg, "Co-authored-by"), Some("C <c@x.test>"));
        assert!(trailers(msg, "Signed-off-by").is_empty());
        assert!(trailers("", "Co-authored-by").is_empty());
        // A key whose length lands inside a multi-byte character must
        // answer "no" rather than panic: a commit message is arbitrary
        // bytes somebody else wrote.
        assert!(trailers("t\n\né: x\n", "ab").is_empty());
    }

    #[test]
    fn keys_fall_back_to_the_commit_oid() {
        let oid = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(key_for("no trailer here", oid), format!("g{oid}"));
        assert_eq!(key_for("t\n\nChange-Id: Icafe1234\n", oid), "Icafe1234");
        // The derived key passes the control plane's shape check.
        assert!(stratum_control::changes::valid_change_key(&derive_key(oid)));
    }
}
