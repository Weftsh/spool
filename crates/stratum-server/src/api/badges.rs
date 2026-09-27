//! The README badge: `build | passing`, rendered here, as an SVG.
//!
//! Every README on earth has one of these and we had none, which is a
//! small thing that reads as a large one — a forge whose projects cannot
//! show their status looks like a forge nobody ships from.
//!
//! ## Rendered here, always
//!
//! No shields.io, no external request of any kind. A badge is embedded
//! in a README, so the URL is fetched by every reader's browser: farming
//! it out to a third party would hand that party a log of who reads
//! whose README, and would make our uptime theirs. The SVG below is
//! about forty lines of string formatting and owes nobody anything.
//!
//! ## Nothing in it comes from a caller
//!
//! The label is the constant `build` and the message comes from a closed
//! set of four words. There is deliberately **no `?label=` parameter**
//! and the branch is never drawn: an SVG is a document, a `<script>` in
//! it runs when the file is opened directly, and git ref names may
//! legitimately contain `<`, `>` and `&`. A badge that echoed its query
//! string would be a stored-XSS vector reachable from any README. So the
//! response bytes are a function of one enum, and `checks_intake_e2e`
//! asserts that hostile input never appears in them.
//!
//! ## What it is actually reporting
//!
//! The checks on the most recent change that **landed** on the branch.
//! Not an open change: an open change is not on the branch, and letting
//! a red review colour trunk's badge would be a badge that lies in the
//! most expensive direction. A branch nothing has landed on — a fresh
//! repository, a mirror, a project whose CI has never reported — is
//! honestly grey, `no status`, never green.

use crate::app::SharedState;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use stratum_control::auth::Scope;
use stratum_control::changes;

/// How far back the badge looks for the branch's last landed change.
///
/// Bounded on purpose: this is a public, uncached-by-us, embedded-in-a-
/// README route, and an unbounded scan behind one is a denial of service
/// with extra steps. The failure mode of the bound is a grey badge on a
/// branch whose last landing is more than 200 changes old, which is the
/// safe direction — it under-claims, never over-claims.
const LOOKBACK: i64 = 200;

/// Colours are hex here, and only here.
///
/// `web/DESIGN.md` forbids a hardcoded hex in a component, and rightly:
/// a stored hex is unfixable when the palette moves. This file is the
/// exception the rule implies — the bytes are served to GitHub, to a
/// docs site, to an editor's markdown preview, none of which has ever
/// heard of `web/shared/tokens.css`. A `var(--ok)` in this SVG renders
/// as black on somebody else's page.
///
/// ## Why these are darker than shields.io's
///
/// The plates are opaque and the text on them is white, so the *inside*
/// of the badge is already independent of whatever page it lands on —
/// that is the whole reason this shape has outlived every other badge
/// design. What is not automatic is the white text staying legible at
/// 11px, and measured against white, shields.io's own palette does not
/// manage it: `brightgreen` #4c1 gives 2.12:1, `yellow` #dfb317 gives
/// 1.98:1, `lightgrey` #9f9f9f gives 2.65:1. All three are below the
/// 3:1 floor, which is why every shields badge carries a blurred dark
/// shadow under each glyph — a legibility crutch for an under-contrasted
/// palette.
///
/// Ours clear that floor everywhere and reach **4.5:1** (WCAG AA for
/// body text) on the two that matter most, while staying at least 2.9:1
/// against GitHub's dark canvas (#0d1117) so the plate never dissolves
/// into the page either. We keep the 1px shadow anyway, because it is
/// what makes the glyph edges crisp at this size — but here it is a
/// finish rather than the thing holding the text up.
///
/// | | white text on it | vs #0d1117 |
/// |---|---|---|
/// | green `#2f7d32` | 5.12:1 | 3.70:1 |
/// | red `#b3261e` | 6.54:1 | 2.90:1 |
/// | amber `#bf8700` | 3.14:1 | 6.02:1 |
/// | grey `#8a8a8a` | 3.45:1 | 5.48:1 |
/// | plate `#555` | 7.46:1 | 2.54:1 |
///
/// **Amber and grey stop at ~3:1 on purpose, and it is a real
/// trade-off, not an oversight.** Both were taken darker first, to
/// 4.86:1 and 5.17:1, and both were then rejected *by looking at them*:
/// a yellow dark enough for 4.5:1 white text is olive, and reads brown
/// rather than "in progress"; a neutral that dark merges with the `#555`
/// label plate beside it, so the badge stops looking like two plates and
/// starts looking like one grey slab. Hue and contrast pull against each
/// other here and legibility of the *state* won.
///
/// Orange (`#b45309`, 5.02:1) was the other candidate for amber and was
/// rejected for a different reason: next to `failing` in the same README
/// row it is too close to the red to tell apart at a glance, and a
/// pending badge that can be mistaken for a failing one is worse than a
/// slightly soft yellow.
const GREY: &str = "#8a8a8a";
const GREEN: &str = "#2f7d32";
const RED: &str = "#b3261e";
const AMBER: &str = "#bf8700";
/// The neutral left plate. #555 is the universal badge convention and it
/// is a neutral, not a brand colour — there is nothing to be gained by
/// being different here, and a reader's eye is calibrated to it.
const PLATE: &str = "#555";

/// The four things a badge may say. A closed set is the point: it is
/// what makes the response bytes independent of the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Passing,
    Failing,
    Pending,
    Unknown,
}

impl Verdict {
    fn message(self) -> &'static str {
        match self {
            Verdict::Passing => "passing",
            Verdict::Failing => "failing",
            Verdict::Pending => "pending",
            Verdict::Unknown => "no status",
        }
    }

    fn color(self) -> &'static str {
        match self {
            Verdict::Passing => GREEN,
            Verdict::Failing => RED,
            Verdict::Pending => AMBER,
            Verdict::Unknown => GREY,
        }
    }
}

/// One patchset's checks, reduced to one word.
///
/// Failing wins over pending wins over passing, and no checks at all is
/// `Unknown` rather than `Passing`. That last line is the whole reason
/// this is a named function with its own test: "nothing reported" and
/// "everything passed" are the same empty list, and reading it as green
/// is how a badge comes to certify a repository nothing has ever built.
pub fn aggregate<'a>(states: impl IntoIterator<Item = &'a str>) -> Verdict {
    let mut seen = false;
    let mut pending = false;
    for s in states {
        seen = true;
        match s {
            "failing" => return Verdict::Failing,
            "pending" => pending = true,
            _ => {}
        }
    }
    match (seen, pending) {
        (false, _) => Verdict::Unknown,
        (true, true) => Verdict::Pending,
        (true, false) => Verdict::Passing,
    }
}

/// Rendered width of one of our five strings at 11px Verdana.
///
/// A table, not an estimate, because the set of strings this badge can
/// draw is closed — five of them — and because a per-character estimate
/// gets the shape visibly wrong. "failing" and "passing" are both seven
/// characters and differ by **8px**: `f`, `i` and `l` are narrow where
/// `p`, `a` and `s` are wide. An estimate that splits the difference
/// makes one badge baggy and the other cramped, which is exactly the
/// tell that a badge was not drawn by the thing everybody else uses.
///
/// The numbers are Verdana's own advance widths, taken from what
/// shields.io computes for these same words, so a Stratum badge sitting
/// next to a shields badge in the same README is the same size to the
/// pixel. `render_metrics_cover_every_string_we_draw` fails if a new
/// string is ever added without a measurement.
fn text_width(s: &str) -> u32 {
    match s {
        "build" => 27,
        "passing" => 41,
        "failing" => 33,
        "pending" => 45,
        "no status" => 51,
        // Unreachable while the strings stay closed, and deliberately
        // generous rather than clever: `textLength` below fits the
        // glyphs to whatever we say here, so an over-estimate is a roomy
        // badge and an under-estimate is squashed text.
        other => (other.chars().count() as u32 * 13).div_ceil(2),
    }
}

/// The left-hand plate's word. A constant, not a parameter — see the
/// module note on why nothing a caller sends is ever drawn.
const LABEL: &str = "build";

/// The badge, as bytes. Pure: same verdict, same SVG, every time.
///
/// The geometry is the flat badge everybody's eye is calibrated to,
/// measured off the real thing rather than remembered: **20px tall,
/// 3px corner radius, 11px Verdana, white text with a 1px dark shadow
/// one pixel below it, and 5px of padding either side of each word.**
/// A shields.io `build|passing` is 88px wide; so is this one.
///
/// Ten pixels of padding — which is what this had before somebody went
/// and measured — made every badge 35% wider than its neighbours in the
/// same README, which is precisely the "visibly homemade" tell.
pub fn render(verdict: Verdict) -> String {
    const PAD: u32 = 5;
    let message = verdict.message();
    let lw = text_width(LABEL) + PAD * 2;
    let mw = text_width(message) + PAD * 2;
    let w = lw + mw;
    let (ltl, mtl) = (text_width(LABEL), text_width(message));
    let (lx, mx) = (lw / 2, lw + mw / 2);
    let color = verdict.color();
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="20" role="img" aria-label="build: {message}">
<title>build: {message}</title>
<linearGradient id="g" x2="0" y2="100%"><stop offset="0" stop-color="#bbb" stop-opacity=".1"/><stop offset="1" stop-opacity=".1"/></linearGradient>
<clipPath id="r"><rect width="{w}" height="20" rx="3"/></clipPath>
<g clip-path="url(#r)">
<rect id="label-plate" width="{lw}" height="20" fill="{PLATE}"/>
<rect id="message-plate" x="{lw}" width="{mw}" height="20" fill="{color}"/>
<rect width="{w}" height="20" fill="url(#g)"/>
</g>
<g fill="#fff" text-anchor="middle" font-family="Verdana,Geneva,DejaVu Sans,sans-serif" text-rendering="geometricPrecision" font-size="11">
<text x="{lx}" y="15" fill="#010101" fill-opacity=".3" textLength="{ltl}" lengthAdjust="spacingAndGlyphs">{LABEL}</text>
<text x="{lx}" y="14" textLength="{ltl}" lengthAdjust="spacingAndGlyphs">{LABEL}</text>
<text x="{mx}" y="15" fill="#010101" fill-opacity=".3" textLength="{mtl}" lengthAdjust="spacingAndGlyphs">{message}</text>
<text x="{mx}" y="14" textLength="{mtl}" lengthAdjust="spacingAndGlyphs">{message}</text>
</g>
</svg>
"##
    )
}

#[derive(Deserialize)]
pub struct BadgeQuery {
    /// Which branch's status. Defaults to the repository's default
    /// branch. Compared, never rendered.
    pub branch: Option<String>,
}

/// GET /v1/orgs/:org/repos/:repo/badge.svg
///
/// Masking is inherited, not reimplemented: `rest_repo_auth` with
/// `RepoRead` is exactly the rule the rest of the API uses, so a public
/// repository answers anyone and a private one answers a stranger the
/// same way a repository that does not exist does. In particular there
/// is no badge that says "private" — that badge would confirm the
/// repository exists, which is the whole thing the masking is for.
pub async fn badge(
    State(state): State<SharedState>,
    Path((org_name, repo_name)): Path<(String, String)>,
    Query(q): Query<BadgeQuery>,
    headers: HeaderMap,
) -> Response {
    let (_, repo, _) = match crate::app::rest_repo_auth(
        &state,
        &headers,
        &org_name,
        &repo_name,
        Scope::RepoRead,
    ) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let branch = q.branch.as_deref().unwrap_or(&repo.default_branch);
    let verdict = match verdict_for(&state, &repo.id, branch) {
        Ok(v) => v,
        Err(e) => return crate::api::internal(e),
    };
    svg_response(verdict)
}

fn verdict_for(state: &SharedState, repo_id: &str, branch: &str) -> Result<Verdict, String> {
    let landed = changes::list(&state.db, repo_id, Some("landed"), LOOKBACK)?;
    // `list` orders by id, which is creation order; the branch's most
    // recent *landing* is the one that moved most recently.
    let Some(newest) = landed
        .into_iter()
        .filter(|c| c.target_branch == branch)
        .max_by_key(|c| c.updated_at)
    else {
        return Ok(Verdict::Unknown);
    };
    let Some(latest) = changes::latest_patchset(&state.db, &newest.id)? else {
        return Ok(Verdict::Unknown);
    };
    let checks = changes::checks_for(&state.db, &latest.id)?;
    Ok(aggregate(checks.iter().map(|c| c.state.as_str())))
}

fn svg_response(verdict: Verdict) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/svg+xml; charset=utf-8"),
            // Sixty seconds. A badge is a claim about right now, and a
            // long max-age is how a badge comes to say "passing" for the
            // rest of the afternoon about a build that went red at
            // eleven. Short enough to be honest, long enough that a
            // popular README does not become a load test.
            (header::CACHE_CONTROL, "public, max-age=60, must-revalidate"),
            // The bytes are ours and they are SVG. Nothing should be
            // guessing at that.
            (
                header::HeaderName::from_static("x-content-type-options"),
                "nosniff",
            ),
        ],
        render(verdict),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_reported_is_grey_and_never_green() {
        // The line this test exists for: an empty check list is the same
        // empty list as "all of them passed", and calling it passing
        // certifies a repository nothing has ever built.
        assert_eq!(aggregate(Vec::<&str>::new()), Verdict::Unknown);
        assert_eq!(aggregate(["passing"]), Verdict::Passing);
        assert_eq!(aggregate(["passing", "passing"]), Verdict::Passing);
        assert_eq!(aggregate(["passing", "pending"]), Verdict::Pending);
        assert_eq!(aggregate(["pending", "passing"]), Verdict::Pending);
        // Failing wins from anywhere in the list, including after a
        // pending that would otherwise have decided it.
        assert_eq!(aggregate(["pending", "failing"]), Verdict::Failing);
        assert_eq!(aggregate(["failing", "pending"]), Verdict::Failing);
        assert_eq!(
            aggregate(["passing", "failing", "passing"]),
            Verdict::Failing
        );
        // A state the enum does not know is not a failure and is not a
        // pass; it counts only as "something reported".
        assert_eq!(aggregate(["weather"]), Verdict::Passing);
    }

    #[test]
    fn every_verdict_renders_a_well_formed_badge_of_its_own_colour() {
        let all = [
            (Verdict::Passing, "passing", GREEN),
            (Verdict::Failing, "failing", RED),
            (Verdict::Pending, "pending", AMBER),
            (Verdict::Unknown, "no status", GREY),
        ];
        let mut widths = Vec::new();
        for (v, word, color) in all {
            let svg = render(v);
            assert!(svg.starts_with("<svg xmlns="), "{svg}");
            assert!(svg.trim_end().ends_with("</svg>"), "{svg}");
            assert!(svg.contains(&format!("fill=\"{color}\"")), "{svg}");
            assert!(svg.contains(&format!(">{word}</text>")), "{svg}");
            assert!(svg.contains("<title>build: "), "{svg}");
            // Every tag opened is closed: an unbalanced SVG renders as
            // nothing at all in a README, which is indistinguishable
            // from the route being broken.
            assert_eq!(
                svg.matches("<text").count(),
                svg.matches("</text>").count(),
                "{svg}"
            );
            assert_eq!(
                svg.matches("<g ").count(),
                svg.matches("</g>").count(),
                "{svg}"
            );
            // The plate and the message together are the declared width,
            // so the coloured half cannot spill past the rounded corner.
            // Read each number back out of the tag that carries it,
            // rather than out of whichever `width="` comes first — the
            // clip rect also has one, and matching that instead is how
            // this assertion quietly stops asserting anything.
            let num = |after: &str, what: &str| -> u32 {
                svg.split(after)
                    .nth(1)
                    .and_then(|s| s.split('"').next())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_else(|| panic!("no {what} in {svg}"))
            };
            let w = num(
                "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"",
                "svg width",
            );
            let lw = num("<rect id=\"label-plate\" width=\"", "label plate width");
            let offset = num("<rect id=\"message-plate\" x=\"", "message plate offset");
            assert_eq!(
                offset, lw,
                "the coloured plate must start where the label ends"
            );
            let mw = num(
                &format!("<rect id=\"message-plate\" x=\"{lw}\" width=\""),
                "message plate width",
            );
            assert_eq!(lw + mw, w, "the two plates must tile the badge exactly");

            // The assertions above are all internally consistent by
            // construction — `w` *is* `lw + mw` — so on their own they
            // would pass against a badge whose text spills out of it.
            // The property that is not tautological is the one between
            // the text and the box it sits in: every run declares a
            // `textLength`, and its plate is wider than that by enough
            // padding to keep the glyphs off the edge. A mutation that
            // halved the padding, and one that dropped `textLength`
            // entirely, both survived until this block existed.
            assert_eq!(
                svg.matches("lengthAdjust=\"spacingAndGlyphs\"").count(),
                svg.matches("<text").count(),
                "every text run must fit itself to its declared length: {svg}"
            );
            let declared: Vec<u32> = svg
                .split("textLength=\"")
                .skip(1)
                .map(|s| s.split('"').next().unwrap().parse().unwrap())
                .collect();
            // Two runs each: the shadow and the text over it.
            assert_eq!(
                declared,
                vec![
                    text_width(LABEL),
                    text_width(LABEL),
                    text_width(word),
                    text_width(word),
                ],
                "{svg}"
            );
            let (ltl, mtl) = (declared[0], declared[2]);
            // 5px either side, which is what every other badge in the
            // README has. Exactness is asserted against shields.io's own
            // output in `the_badge_is_pixel_identical_in_size_to_a_shields_io_badge`;
            // this one just refuses to let the text touch the edge.
            const MIN_PAD: u32 = 10;
            assert!(
                lw >= ltl + MIN_PAD,
                "the label is jammed against the badge edge: {lw} vs {ltl}"
            );
            assert!(
                mw >= mtl + MIN_PAD,
                "the message is jammed against the badge edge: {mw} vs {mtl}"
            );
            widths.push((word, w));
        }
        // A longer word gets a wider badge — otherwise `textLength`
        // would be squeezing "no status" into the width of "passing".
        let passing = widths.iter().find(|(w, _)| *w == "passing").unwrap().1;
        let unknown = widths.iter().find(|(w, _)| *w == "no status").unwrap().1;
        assert!(unknown > passing, "{widths:?}");
    }

    #[test]
    fn the_rendered_bytes_depend_on_nothing_but_the_verdict() {
        // The property the module note claims: no caller-supplied text
        // reaches the SVG, so there is no escaping to get wrong. If a
        // `?label=` or a drawn branch name is ever added, this fails.
        for v in [
            Verdict::Passing,
            Verdict::Failing,
            Verdict::Pending,
            Verdict::Unknown,
        ] {
            assert_eq!(render(v), render(v));
            let svg = render(v);
            assert!(!svg.contains("<script"), "{svg}");
            for word in svg
                .split('>')
                .filter_map(|s| s.split('<').next())
                .filter(|s| !s.trim().is_empty())
            {
                assert!(
                    word.trim() == "build"
                        || word.trim() == v.message()
                        || word.starts_with("build: "),
                    "unexpected text {word:?} in the badge"
                );
            }
        }
    }

    /// The badge is the size shields.io would have drawn it.
    ///
    /// These five numbers are Verdana's advance widths for our five
    /// strings, and the totals below are what `img.shields.io/badge/
    /// build-<word>-<colour>` actually serves. A badge that is 35% wider
    /// than every other badge in the README is the tell that it was not
    /// drawn by the thing everybody else uses, and that is exactly what
    /// this was before somebody measured: 10px of padding a side instead
    /// of 5, and a per-character estimate instead of real metrics.
    #[test]
    fn the_badge_is_pixel_identical_in_size_to_a_shields_io_badge() {
        assert_eq!(text_width(LABEL), 27, "\"build\" at 11px Verdana");
        for (v, word, width, total) in [
            (Verdict::Passing, "passing", 41, 88),
            (Verdict::Failing, "failing", 33, 80),
            (Verdict::Pending, "pending", 45, 92),
            (Verdict::Unknown, "no status", 51, 98),
        ] {
            assert_eq!(text_width(word), width, "{word}");
            // Asserted against what `render` actually draws, not against
            // the padding arithmetic restated — restating it is how this
            // assertion survived the padding going back to 10px a side.
            let svg = render(v);
            assert!(
                svg.starts_with(&format!(
                    "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{total}\" height=\"20\""
                )),
                "{word}: shields serves this badge at {total}px wide, we drew {}",
                svg.lines().next().unwrap_or_default()
            );
        }
        // "failing" and "passing" are both seven characters and differ
        // by 8px. Any per-character estimate gets one of them wrong, and
        // this is the assertion that refuses to let one back in.
        assert_ne!(text_width("failing"), text_width("passing"));
    }

    /// Relative luminance, so the palette can be asserted rather than
    /// restated. Comparing a colour constant against itself read back
    /// out of the SVG proves the formatter works and nothing about
    /// whether anybody can read the badge.
    fn luminance(hex: &str) -> f64 {
        let h = hex.trim_start_matches('#');
        let h: String = if h.len() == 3 {
            h.chars().flat_map(|c| [c, c]).collect()
        } else {
            h.to_string()
        };
        let chan = |i: usize| {
            let c = u8::from_str_radix(&h[i..i + 2], 16).expect("hex") as f64 / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * chan(0) + 0.7152 * chan(2) + 0.0722 * chan(4)
    }

    fn contrast(a: &str, b: &str) -> f64 {
        let (x, y) = (luminance(a), luminance(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    /// Saturation — how far a colour is from grey. 0 for any neutral.
    fn chroma(hex: &str) -> f64 {
        let h = hex.trim_start_matches('#');
        let h: String = if h.len() == 3 {
            h.chars().flat_map(|c| [c, c]).collect()
        } else {
            h.to_string()
        };
        let ch = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).expect("hex") as f64 / 255.0;
        let (r, g, b) = (ch(0), ch(2), ch(4));
        r.max(g).max(b) - r.min(g).min(b)
    }

    /// The palette has to survive a page we do not control.
    ///
    /// A badge is embedded in somebody else's README, on a background
    /// nobody here chose, and every clause below is a colour this file
    /// got wrong at some point and had to be looked at in a browser to
    /// notice. They are asserted as *properties* rather than by reading
    /// the constants back out of the SVG — that version passed against
    /// both bad colours.
    #[test]
    fn the_palette_survives_a_page_we_do_not_control() {
        // 1. White text stays legible on every plate. shields.io's own
        //    palette fails this (`yellow` #dfb317 is 1.98:1) and leans
        //    on a blurred drop shadow to compensate.
        // 2. The badge does not dissolve into the page, light or dark.
        for v in [
            Verdict::Passing,
            Verdict::Failing,
            Verdict::Pending,
            Verdict::Unknown,
        ] {
            let (c, m) = (v.color(), v.message());
            assert!(
                contrast(c, "#ffffff") >= 3.0,
                "{m}: white text on {c} is {:.2}:1, below the 3:1 floor",
                contrast(c, "#ffffff")
            );
            for page in ["#ffffff", "#0d1117"] {
                assert!(
                    contrast(c, page) >= 2.5,
                    "{m}: {c} on a {page} page is {:.2}:1 — the badge \
                     dissolves into the page it is embedded in",
                    contrast(c, page)
                );
            }
        }
        for page in ["#ffffff", "#0d1117"] {
            assert!(contrast(PLATE, page) >= 2.5, "{PLATE} on {page}");
        }
        assert!(contrast(PLATE, "#ffffff") >= 4.5, "white text on the plate");

        // 3. The two plates read as *two*. For the coloured states hue
        //    does that work, so the requirement on them is that they are
        //    actually coloured — a state greyed out to win a contrast
        //    argument stops being a state. The neutral shares its hue
        //    with the label plate, so for that one it has to be
        //    lightness, and #6d6d6d — tried first, and 5.17:1 on white
        //    text — turned the badge into a single grey slab.
        assert!(chroma(PLATE) < 0.05, "the label plate must be neutral");
        for v in [Verdict::Passing, Verdict::Failing, Verdict::Pending] {
            assert!(
                chroma(v.color()) >= 0.25,
                "{}: {} is too close to grey to read as a state",
                v.message(),
                v.color()
            );
        }
        assert!(
            contrast(Verdict::Unknown.color(), PLATE) >= 1.8,
            "the neutral {} against the {PLATE} plate is only {:.2}:1 — \
             the badge reads as one slab rather than two plates",
            Verdict::Unknown.color(),
            contrast(Verdict::Unknown.color(), PLATE)
        );

        // 4. Amber reads as amber. This is the one clause that is a
        //    judgement rather than a ratio, so it is worth saying why the
        //    number is where it is: yellow is intrinsically light, and a
        //    yellow taken dark enough for 4.5:1 white text is olive — it
        //    reads brown, which is not "in progress". #96690f cleared
        //    every other assertion here and still had to be rejected by
        //    looking at it. A luminance floor is what that judgement
        //    reduces to.
        assert!(
            luminance(AMBER) >= 0.20,
            "{AMBER} has luminance {:.3}; below ~0.20 an amber reads as \
             olive-brown rather than as a build in progress",
            luminance(AMBER)
        );
        // …and it must not slide the other way into the red's territory,
        // because a pending badge mistaken for a failing one is the
        // expensive confusion. Orange #b45309 was rejected for this.
        let g = |hex: &str| u8::from_str_radix(&hex.trim_start_matches('#')[2..4], 16).unwrap();
        assert!(
            g(AMBER) as i32 - g(RED) as i32 >= 60,
            "{AMBER} is too close in hue to {RED} to tell apart at a glance"
        );
    }

    /// Every string the badge can draw has a measurement.
    ///
    /// The fallback estimate exists so the function is total, not so it
    /// can be relied on. A fifth word arriving without a measured width
    /// would silently get an estimate and render at the wrong size.
    #[test]
    fn render_metrics_cover_every_string_we_draw() {
        let estimate = |s: &str| (s.chars().count() as u32 * 13).div_ceil(2);
        assert_ne!(
            text_width(LABEL),
            estimate(LABEL),
            "the label is falling through to the estimate"
        );
        for v in [
            Verdict::Passing,
            Verdict::Failing,
            Verdict::Pending,
            Verdict::Unknown,
        ] {
            let m = v.message();
            assert_ne!(
                text_width(m),
                estimate(m),
                "{m:?} has no measured width and is falling through to the estimate"
            );
        }
        // The fallback is still there, and still generous rather than
        // clever — an unmeasured string gets a roomy badge, never a
        // squashed one.
        assert!(text_width("something else") >= "something else".len() as u32 * 6);
    }
}
