import { describe, expect, it } from "vitest";
import type { License, RefName, RepoMeta } from "@/api";
import {
  compareVersions,
  countRows,
  healthRows,
  homepageLabel,
  latestTag,
  licenseLabel,
  overflow,
  parseVersionTag,
  type HealthKind,
} from "./about";

// The About rail is a repository's calling card, and almost every
// honesty question it raises is answerable without a DOM: whether an
// absent health file survives as an *absence* rather than as a missing
// row, and whether "latest" means anything we can defend. Those are the tests
// worth having, so the logic lives in the module and the components
// above it are arrangement.

function meta(over: Partial<RepoMeta> = {}): RepoMeta {
  return {
    topics: [],
    languages: [],
    languages_truncated: false,
    license: null,
    community: [],
    readme: null,
    ...over,
  };
}

function license(over: Partial<License> = {}): License {
  return {
    path: "LICENSE",
    spdx: "MIT",
    name: "MIT License",
    recognised: true,
    files: ["LICENSE"],
    ...over,
  };
}

function tag(name: string): RefName {
  return {
    name,
    full: `refs/tags/${name}`,
    oid: "0".repeat(40),
    default: false,
  };
}

const ORDER: HealthKind[] = [
  "readme",
  "license",
  "code_of_conduct",
  "contributing",
  "security",
  "activity",
];

describe("healthRows", () => {
  it("renders all six rows for a repository that has nothing", () => {
    // The whole reason this function exists. GitHub omits the rows it
    // has no file for, so a project with no code of conduct and no
    // security policy looks identical to one with both. Every row is
    // present as a row, and absent as a fact.
    const rows = healthRows(meta());
    expect(rows.map((r) => r.kind)).toEqual(ORDER);
    expect(
      rows.filter((r) => r.kind !== "activity").map((r) => r.present),
    ).toEqual([false, false, false, false, false]);
    expect(rows.map((r) => r.paths)).toEqual([[], [], [], [], [], []]);
  });

  it("keeps the order when every file is present", () => {
    const rows = healthRows(
      meta({
        readme: "README.md",
        license: license(),
        community: [
          // Deliberately not in the rendered order: the server sends
          // what it found, and the rail's order is the rail's.
          { kind: "security", path: ".github/SECURITY.md" },
          { kind: "contributing", path: "CONTRIBUTING.md" },
          { kind: "code_of_conduct", path: "docs/CODE_OF_CONDUCT.md" },
        ],
      }),
    );
    expect(rows.map((r) => r.kind)).toEqual(ORDER);
    expect(rows.every((r) => r.present)).toBe(true);
    expect(rows.map((r) => r.paths)).toEqual([
      ["README.md"],
      ["LICENSE"],
      ["docs/CODE_OF_CONDUCT.md"],
      ["CONTRIBUTING.md"],
      [".github/SECURITY.md"],
      [],
    ]);
  });

  it("counts an absent readme as absent rather than as a file named nothing", () => {
    const rows = healthRows(meta({ readme: null }));
    expect(rows[0]).toEqual({
      kind: "readme",
      label: "Readme",
      paths: [],
      present: false,
    });
  });

  it("keeps activity present with no file behind it", () => {
    // Not a health file — it is the repository's own history, which
    // exists the moment the repository does. If this ever renders
    // "None" the rail is telling a reader the project has no history.
    const rows = healthRows(meta());
    expect(rows[5]).toEqual({
      kind: "activity",
      label: "Activity",
      paths: [],
      present: true,
    });
  });

  it("lists every licence file of a dual-licensed project", () => {
    const rows = healthRows(
      meta({
        license: license({
          path: null,
          spdx: null,
          recognised: false,
          files: ["LICENSE-APACHE", "LICENSE-MIT"],
        }),
      }),
    );
    expect(rows[1].present).toBe(true);
    expect(rows[1].paths).toEqual(["LICENSE-APACHE", "LICENSE-MIT"]);
    expect(rows[1].label).toBe("2 licenses found");
  });

  it("ignores a community kind this bundle has not heard of", () => {
    // A `kind` the page does not know means the server is newer than the
    // bundle, which is a deploy skew and not a reason to blank a row or
    // to throw.
    const rows = healthRows(
      meta({
        community: [
          { kind: "governance" as "security", path: "GOVERNANCE.md" },
          { kind: "security", path: "SECURITY.md" },
        ],
      }),
    );
    expect(rows.map((r) => r.kind)).toEqual(ORDER);
    expect(rows[4].paths).toEqual(["SECURITY.md"]);
  });
});

describe("licenseLabel", () => {
  it("says the plain word when there is no licence", () => {
    expect(licenseLabel(null)).toBe("License");
  });

  it("uses the SPDX id, which is the string a reader arrives looking for", () => {
    expect(licenseLabel(license({ spdx: "Apache-2.0" }))).toBe(
      "Apache-2.0 license",
    );
  });

  it("says an unrecognised licence is unrecognised rather than guessing", () => {
    expect(
      licenseLabel(license({ recognised: false, spdx: null, name: null })),
    ).toBe("License (unrecognised)");
  });

  it("believes `recognised` over a leftover SPDX id", () => {
    // The flag is the server's answer and the id is only the wording. A
    // row that says "we did not recognise this" while still carrying an
    // id from an earlier classification must not be rendered as a
    // confident `MIT license` — naming the wrong licence is the one
    // mistake this row can make that a reader will act on.
    expect(licenseLabel(license({ recognised: false, spdx: "MIT" }))).toBe(
      "License (unrecognised)",
    );
  });

  it("refuses to name a licence when the SPDX id is missing", () => {
    // `recognised` without an id is a server that changed its mind
    // halfway. `undefined license` is worse than saying we do not know.
    expect(licenseLabel(license({ recognised: true, spdx: null }))).toBe(
      "License (unrecognised)",
    );
  });

  it("counts rather than picks when a project carries several", () => {
    expect(
      licenseLabel(license({ files: ["LICENSE-APACHE", "LICENSE-MIT"] })),
    ).toBe("2 licenses found");
  });
});

describe("parseVersionTag", () => {
  it("reads the ordinary shapes", () => {
    expect(parseVersionTag("v1.2.3")).toEqual({
      major: 1,
      minor: 2,
      patch: 3,
      pre: null,
    });
    expect(parseVersionTag("1.2.3")).toEqual({
      major: 1,
      minor: 2,
      patch: 3,
      pre: null,
    });
    expect(parseVersionTag("V1.2.3")).toEqual({
      major: 1,
      minor: 2,
      patch: 3,
      pre: null,
    });
  });

  it("defaults a missing patch to zero", () => {
    expect(parseVersionTag("v2.1")).toEqual({
      major: 2,
      minor: 1,
      patch: 0,
      pre: null,
    });
  });

  it("keeps the prerelease and drops the build metadata", () => {
    // Semver: build metadata takes no part in precedence, so carrying it
    // into the comparison would order two identical releases.
    expect(parseVersionTag("1.0.0-rc.1")?.pre).toBe("rc.1");
    expect(parseVersionTag("1.0.0+build.7")?.pre).toBe(null);
    expect(parseVersionTag("1.0.0-rc.1+build.7")?.pre).toBe("rc.1");
  });

  it("refuses a bare number, which is a year or a build and not a version", () => {
    // The failure this prevents: `2024` parsed as `2024.0.0` jumps over
    // every real release the project has ever tagged.
    expect(parseVersionTag("2024")).toBe(null);
    expect(parseVersionTag("v9")).toBe(null);
  });

  it("refuses a word", () => {
    expect(parseVersionTag("nightly")).toBe(null);
    expect(parseVersionTag("release-candidate")).toBe(null);
    expect(parseVersionTag("v1.2.3.4")).toBe(null);
  });
});

describe("compareVersions", () => {
  const v = (s: string) => {
    const parsed = parseVersionTag(s);
    if (!parsed) throw new Error(`not a version: ${s}`);
    return parsed;
  };
  const older = (a: string, b: string) =>
    expect(Math.sign(compareVersions(v(a), v(b)))).toBe(-1);

  it("orders by major, then minor, then patch", () => {
    older("1.9.9", "2.0.0");
    older("1.2.9", "1.3.0");
    older("1.2.3", "1.2.4");
  });

  it("calls two spellings of the same version equal", () => {
    expect(compareVersions(v("v1.2.3"), v("1.2.3"))).toBe(0);
    expect(compareVersions(v("1.2.0-rc.1"), v("v1.2.0-rc.1"))).toBe(0);
  });

  it("ranks a release above its own prereleases", () => {
    // The one rule people would notice us getting backwards.
    older("1.0.0-rc.9", "1.0.0");
  });

  it("compares numeric prerelease identifiers as numbers", () => {
    // The reason `comparePre` is written out rather than being a string
    // comparison: as strings, "rc.10" sorts before "rc.2".
    older("1.0.0-rc.2", "1.0.0-rc.10");
  });

  it("ranks a numeric identifier below an alphanumeric one", () => {
    older("1.0.0-1", "1.0.0-alpha");
  });

  it("compares alphanumeric identifiers alphabetically", () => {
    older("1.0.0-alpha", "1.0.0-beta");
  });

  it("ranks a shorter identifier list below a longer one that shares its prefix", () => {
    older("1.0.0-alpha", "1.0.0-alpha.1");
  });
});

describe("latestTag", () => {
  it("picks the greatest version, whatever order the server sent", () => {
    expect(latestTag([tag("v1.0.0"), tag("v2.1.0"), tag("v0.9.0")])?.name).toBe(
      "v2.1.0",
    );
    expect(
      latestTag([tag("v2.1.0"), tag("v1.0.0"), tag("v10.0.0")])?.name,
    ).toBe("v10.0.0");
  });

  it("answers nothing when no tag names a version", () => {
    // A `RefName` carries no date, so with nothing version-shaped to
    // order there is no honest answer — and an arbitrary one wearing a
    // "Latest" badge is worse than no badge.
    expect(latestTag([tag("nightly"), tag("ship-it"), tag("2024-06-01")])).toBe(
      null,
    );
  });

  it("answers nothing for a repository with no tags at all", () => {
    expect(latestTag([])).toBe(null);
  });

  it("ignores the tags that are not versions and orders the ones that are", () => {
    expect(
      latestTag([tag("nightly"), tag("v1.4.0"), tag("stable"), tag("v1.3.0")])
        ?.name,
    ).toBe("v1.4.0");
  });

  it("prefers a release to a later-listed prerelease of itself", () => {
    expect(latestTag([tag("v3.0.0"), tag("v3.0.0-rc.1")])?.name).toBe("v3.0.0");
  });

  it("keeps the first of two equal spellings, so the answer is stable", () => {
    expect(latestTag([tag("v1.2.3"), tag("1.2.3")])?.name).toBe("v1.2.3");
  });
});

describe("overflow", () => {
  it("shows everything when the list fits", () => {
    expect(overflow([1, 2, 3], 5)).toEqual({ shown: [1, 2, 3], extra: 0 });
    expect(overflow([1, 2, 3], 3)).toEqual({ shown: [1, 2, 3], extra: 0 });
  });

  it("never hides exactly one item", () => {
    // A "+1" takes about as much room as the thing it stands for, so
    // trading the last topic for a badge saying there is one more topic
    // is a worse rail and an ungenerous one.
    expect(overflow([1, 2, 3, 4], 3)).toEqual({
      shown: [1, 2, 3, 4],
      extra: 0,
    });
  });

  it("cuts to the cap once two or more would be hidden", () => {
    expect(overflow([1, 2, 3, 4, 5], 3)).toEqual({
      shown: [1, 2, 3],
      extra: 2,
    });
  });

  it("handles an empty list", () => {
    expect(overflow([], 3)).toEqual({ shown: [], extra: 0 });
  });
});

describe("countRows", () => {
  it("names the counts in FORGE-UX's order", () => {
    expect(
      countRows({ watchers: 12, forks: 0 }).map((r) => [
        r.kind,
        r.value,
        r.label,
      ]),
    ).toEqual([
      ["watching", 12, "watching"],
      ["forks", 0, "forks"],
    ]);
  });

  it("uses the singular for exactly one", () => {
    const rows = countRows({ watchers: 1, forks: 1 });
    expect(rows.map((r) => r.label)).toEqual(["watching", "fork"]);
  });

  it("renders zero rather than hiding the row", () => {
    // A project nobody has forked is a fact about the project. A missing
    // row reads as a missing feature.
    expect(countRows({ watchers: 0, forks: 0 })).toHaveLength(2);
  });
});

describe("homepageLabel", () => {
  it("drops the scheme and the trailing slash", () => {
    expect(homepageLabel("https://example.org/")).toBe("example.org");
    expect(homepageLabel("http://example.org/docs")).toBe("example.org/docs");
    expect(homepageLabel("  https://example.org  ")).toBe("example.org");
  });

  it("leaves a path alone apart from its trailing slash", () => {
    expect(homepageLabel("https://example.org/a/b/")).toBe("example.org/a/b");
  });
});
