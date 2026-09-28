/// The handle the server will make for somebody who does not choose one.
///
/// Accepting an invitation as a new person asks for a name and a
/// password; the handle — the `you` in `/you/repo`, which goes in every
/// clone URL the person hands out — is optional. Left out, the server
/// makes it from the part of the invited address before the `@`. The
/// form shows that name *before* anybody submits, so the person can see
/// what they will get and type something else if they would rather.
///
/// A copy of `handle_from` in `crates/stratum-control/src/registry.rs`,
/// rule for rule: lowercase; every run of characters outside
/// `[a-z0-9_]` becomes one `-`; leading and trailing `-`/`_` trimmed;
/// at most 60 characters, and a cut never leaves a trailing `-`/`_`;
/// nothing usable left is `user`. The unit tests are the Rust tests'
/// cases, so a change to one that is not made to the other shows up as
/// a red test rather than as a hint that promises one name and an
/// account that gets another.
///
/// It is a *hint*, not a promise, and the form says so: the server adds
/// a short suffix when the name is already taken or reserved, which only
/// it can know.
export function handleFrom(seed: string): string {
  let out = "";
  // `for…of` walks code points, not UTF-16 units, so a character outside
  // the basic plane is one character here as it is in Rust. (Runs of
  // them collapse to one dash either way, but the cut at 60 would not.)
  for (const ch of seed.toLowerCase()) {
    const c = /^[a-z0-9_]$/.test(ch) ? ch : "-";
    // One dash for a run of anything else: `a..b` is `a-b`.
    if (!(c === "-" && out.endsWith("-"))) out += c;
  }
  const cut = out.replace(/^[-_]+|[-_]+$/g, "").slice(0, 60);
  const trimmed = cut.replace(/[-_]+$/, "");
  return trimmed === "" ? "user" : trimmed;
}

/// The handle the server derives for an invited address: `handleFrom`
/// of the part before the first `@`, exactly as the server splits it.
export function handleFromAddress(email: string): string {
  return handleFrom(email.split("@")[0]);
}
