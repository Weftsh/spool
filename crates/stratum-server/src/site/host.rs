//! The hostname a site is served at, and how it is derived from the
//! names of the org and the repository.
//!
//! This cannot be a formatting detail computed at serve time, and the
//! reason is that our own naming rules and DNS's do not agree.
//! `registry::valid_name` admits `_` and `.` and up to 100 characters;
//! a DNS label admits neither of those characters, may not begin or end
//! with a hyphen, and stops at 63 octets. A repository legitimately
//! called `my_site.v2` has no literal hostname, and one called `.` a
//! hundred times over would have a label longer than DNS will carry.
//!
//! So the label is **derived once, stored, and thereafter authoritative**:
//! the lookup on a request is an exact match against a column, never a
//! parse of a hostname back into two names. That matters twice over.
//! Parsing would be ambiguous the moment a repository is called `a--b`,
//! and a stored label is one a person can be shown, kept stable across
//! a rename, and eventually chosen for themselves without changing
//! anything about how a request is served.
//!
//! A site's URL is the most permanent thing this feature hands out.
//! Every link to it is somebody else's, so the derivation is written to
//! be boring and the result is written down.

/// The longest a single DNS label may be, in octets. Labels are ASCII
/// here by construction, so octets and characters are the same thing.
pub const MAX_LABEL: usize = 63;

/// What separates the repository from the org. Doubled so that a single
/// hyphen stays available inside either name without the join becoming
/// ambiguous to a reader.
pub const SEP: &str = "--";

/// What a part slugs to when the name had nothing a DNS label can
/// carry — a repository named `___`, say. Never empty, because an empty
/// part would produce a label with a leading or trailing separator.
const EMPTY_PART: &str = "x";

/// Reduce one org or repository name to something a DNS label can hold.
///
/// Lowercased because DNS is case-insensitive and a stored label that
/// differs from the request only in case would never match. Every
/// character DNS will not carry becomes a hyphen, runs of hyphens
/// collapse to one, and the ends are trimmed — a label may not begin or
/// end with a hyphen.
///
/// This is lossy on purpose, and collisions it creates are resolved by
/// the caller against what is already taken, not by making the rule
/// cleverer. `my_site` and `my-site` and `my.site` all want the same
/// label and only one of them can have it; which one is a question about
/// the database, not about the string.
pub fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_dash = true; // true so a leading run is dropped
    for c in name.chars() {
        let c = c.to_ascii_lowercase();
        let keep = c.is_ascii_alphanumeric();
        if keep {
            out.push(c);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        return EMPTY_PART.to_string();
    }
    out
}

/// Truncate to `n` characters without leaving a trailing hyphen behind,
/// which would be an illegal label ending.
fn cut(s: &str, n: usize) -> String {
    let mut t: String = s.chars().take(n).collect();
    while t.ends_with('-') {
        t.pop();
    }
    if t.is_empty() {
        return EMPTY_PART.to_string();
    }
    t
}

/// The label for `org`/`repo`, before any collision is resolved.
///
/// The budget is split between the two parts rather than truncating the
/// join, so a long org name cannot erase the repository name entirely
/// and leave every repository in that org wanting one label. The
/// repository is given the larger share when both are long, because it
/// is the more specific of the two and the one a reader is looking for.
pub fn label(org: &str, repo: &str) -> String {
    let (o, r) = (slug(org), slug(repo));
    let budget = MAX_LABEL - SEP.len();
    if o.len() + r.len() <= budget {
        return format!("{r}{SEP}{o}");
    }
    // Give each part half, then hand any share the other did not use
    // back, so a short org beside a very long repository still spends
    // the whole budget.
    let half = budget / 2;
    let (mut ro, mut rr) = (half, budget - half);
    if o.len() < ro {
        rr += ro - o.len();
        ro = o.len();
    } else if r.len() < rr {
        ro += rr - r.len();
        rr = r.len();
    }
    format!("{}{SEP}{}", cut(&r, rr), cut(&o, ro))
}

/// The `n`th candidate label for `org`/`repo`: the plain one at 0, then
/// `-2`, `-3` and so on, each still a legal label.
///
/// Suffixing rather than hashing because the result is a URL a person
/// reads and types. `docs--acme-2` is recognisably the second `docs`;
/// `docs--acme-f3a91c` is not recognisably anything.
pub fn candidate(org: &str, repo: &str, n: usize) -> String {
    let base = label(org, repo);
    if n == 0 {
        return base;
    }
    let suffix = format!("-{}", n + 1);
    let room = MAX_LABEL - suffix.len();
    format!("{}{suffix}", cut(&base, room))
}

/// Is this a label we could have produced and DNS will carry?
///
/// Used to hold stored labels to the same rule the generator follows, so
/// a value that reached the column another way — a future route that
/// lets somebody choose their own — cannot be one that fails to resolve.
pub fn is_valid_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_LABEL
        && !s.starts_with('-')
        && !s.ends_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_name_is_its_own_slug() {
        assert_eq!(slug("docs"), "docs");
        assert_eq!(slug("my-site"), "my-site");
        assert_eq!(slug("site2"), "site2");
    }

    /// The characters our own naming rules allow and DNS does not. This
    /// is the reason the module exists.
    #[test]
    fn the_characters_dns_will_not_carry_become_hyphens() {
        assert_eq!(slug("my_site"), "my-site");
        assert_eq!(slug("my.site"), "my-site");
        assert_eq!(slug("my_site.v2"), "my-site-v2");
    }

    #[test]
    fn case_is_folded_because_dns_does_not_carry_it() {
        assert_eq!(slug("MySite"), "mysite");
        assert_eq!(slug("ACME"), "acme");
    }

    #[test]
    fn a_label_may_not_begin_or_end_with_a_hyphen() {
        assert_eq!(slug("-lead"), "lead");
        assert_eq!(slug("trail-"), "trail");
        assert_eq!(slug("_both_"), "both");
        assert_eq!(slug("a__b"), "a-b");
        assert_eq!(slug("a..b"), "a-b");
    }

    /// `valid_name` admits a name made only of characters DNS drops, so
    /// the slug of one is empty and would produce `--org`.
    #[test]
    fn a_name_with_nothing_dns_can_carry_still_slugs_to_something() {
        assert_eq!(slug("___"), EMPTY_PART);
        assert_eq!(slug("_"), EMPTY_PART);
        assert!(is_valid_label(&label("acme", "___")));
    }

    #[test]
    fn the_repository_comes_first_because_it_is_what_a_reader_looks_for() {
        assert_eq!(label("acme", "docs"), "docs--acme");
    }

    /// Both names may be 100 characters, so the join may not simply be
    /// formatted and hoped for.
    #[test]
    fn a_label_never_exceeds_what_dns_will_carry() {
        let long = "a".repeat(100);
        let l = label(&long, &long);
        assert!(l.len() <= MAX_LABEL, "{} > {MAX_LABEL}", l.len());
        assert!(is_valid_label(&l), "{l}");
    }

    /// A long org must not erase the repository name, or every
    /// repository in that org would want one label.
    #[test]
    fn both_parts_survive_when_one_of_them_is_long() {
        let long = "o".repeat(100);
        let l = label(&long, "docs");
        assert!(l.starts_with("docs--"), "{l}");
        assert!(l.len() <= MAX_LABEL);

        let l2 = label("acme", &"r".repeat(100));
        assert!(l2.ends_with("--acme"), "{l2}");
        assert!(l2.len() <= MAX_LABEL);
    }

    /// Truncation must not leave a trailing hyphen behind, which is an
    /// illegal way for a DNS label to end. Reached when the cut lands
    /// exactly on one — a repository whose name has a hyphen right at
    /// the budget.
    #[test]
    fn truncating_onto_a_hyphen_drops_it() {
        // 4 for "acme", 2 for the separator, so the repository gets 57.
        // Put the hyphen at character 57 so the cut lands on it.
        let repo = format!("{}-{}", "b".repeat(56), "c".repeat(20));
        let l = label("acme", &repo);
        assert_eq!(l, format!("{}--acme", "b".repeat(56)));
        assert!(!l.starts_with('-') && !l.contains("---"), "{l}");
        assert!(is_valid_label(&l), "{l}");
    }

    #[test]
    fn a_short_part_gives_its_unused_room_to_the_long_one() {
        let l = label("acme", &"r".repeat(100));
        // 63 - 2 separator - 4 "acme" = 57 for the repo part.
        assert_eq!(l, format!("{}--acme", "r".repeat(57)));
    }

    #[test]
    fn the_first_candidate_is_the_plain_label() {
        assert_eq!(candidate("acme", "docs", 0), "docs--acme");
    }

    /// Readable, because it is a URL somebody types.
    #[test]
    fn a_collision_counts_up_and_stays_recognisable() {
        assert_eq!(candidate("acme", "docs", 1), "docs--acme-2");
        assert_eq!(candidate("acme", "docs", 2), "docs--acme-3");
    }

    #[test]
    fn a_suffixed_candidate_still_fits_and_is_still_legal() {
        let long = "a".repeat(100);
        for n in [0, 1, 9, 98, 999] {
            let c = candidate(&long, &long, n);
            assert!(c.len() <= MAX_LABEL, "{n}: {} > {MAX_LABEL}", c.len());
            assert!(is_valid_label(&c), "{n}: {c}");
        }
    }

    #[test]
    fn validation_agrees_with_what_generation_produces() {
        for (org, repo) in [
            ("acme", "docs"),
            ("ACME", "my_site.v2"),
            ("_", "___"),
            ("-x-", "-y-"),
            (&"o".repeat(100), &"r".repeat(100)),
        ] {
            for n in [0, 1, 5] {
                let c = candidate(org, repo, n);
                assert!(is_valid_label(&c), "{org}/{repo}#{n} produced {c:?}");
            }
        }
    }

    #[test]
    fn validation_refuses_what_dns_would_refuse() {
        for bad in [
            "",
            "-lead",
            "trail-",
            "Upper",
            "under_score",
            "dot.ted",
            &"a".repeat(MAX_LABEL + 1),
        ] {
            assert!(!is_valid_label(bad), "{bad:?} should be invalid");
        }
    }

    /// The lossy step, stated as a test so nobody is surprised by it:
    /// three different repositories want one label, and the database is
    /// what decides between them.
    #[test]
    fn different_names_can_want_the_same_label() {
        assert_eq!(label("acme", "my_site"), label("acme", "my-site"));
        assert_eq!(label("acme", "my.site"), label("acme", "my-site"));
    }
}
