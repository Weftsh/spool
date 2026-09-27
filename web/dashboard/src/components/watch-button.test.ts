import { describe, expect, it } from "vitest";
import type { WatchLevel } from "@/api";
import { WATCH_ITEMS, watchLabel } from "./watch-button";

const LEVELS: WatchLevel[] = ["all", "participating", "ignore"];

describe("the watch menu", () => {
  it("offers every level exactly once, and nothing else", () => {
    // The menu is the only way a level is reachable, so a level missing
    // from this list is a subscription nobody can choose — and a
    // duplicate is two rows that would both draw a check.
    expect(WATCH_ITEMS.map((i) => i.level).sort()).toEqual([...LEVELS].sort());
  });

  it("opens on the default rather than burying it", () => {
    // participating is what the server returns for somebody who has
    // never touched this, so it is the row most people are looking at
    // their own state in. GitHub puts it first for the same reason.
    expect(WATCH_ITEMS[0].level).toBe("participating");
  });

  it("says what each choice will do, in a sentence", () => {
    for (const item of WATCH_ITEMS) {
      expect(item.title.length, item.level).toBeGreaterThan(0);
      expect(item.description.endsWith("."), item.level).toBe(true);
    }
  });

  it("promises review notifications, which OWNERS makes true", () => {
    // Not GitHub's wording, and not decoration: the claim is only
    // honest because a change's required reviewers are computed from
    // the OWNERS file. If that ever stops being so, this sentence is
    // the thing that has to change.
    const participating = WATCH_ITEMS.find((i) => i.level === "participating")!;
    expect(participating.description).toContain("need your review");
  });

  it("has no Custom row", () => {
    // Deliberately absent: it leads to a dialog with nothing behind it.
    expect(WATCH_ITEMS.map((i) => i.title)).not.toContain("Custom");
  });
});

describe("watchLabel", () => {
  it("names every level with something to render", () => {
    // A total mapping, checked as one: a level added to the type
    // without a word here would leave the button blank, and a blank
    // button is one the walkthrough's audit fails as unlabelled.
    for (const level of LEVELS)
      expect(watchLabel(level).length).toBeGreaterThan(0);
  });

  it("invites, then reports", () => {
    expect(watchLabel("participating")).toBe("Watch");
    expect(watchLabel("all")).toBe("Watching");
    expect(watchLabel("ignore")).toBe("Ignoring");
  });
});
