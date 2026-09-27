import { describe, expect, it } from "vitest";
import {
  TOKEN_EXPIRY_NOTE,
  groupAccessLine,
  orderedLabels,
  registrationCommands,
  runnerJobHref,
  runnerStatePresentation,
} from "./runners";

describe("runnerStatePresentation", () => {
  it("gives each state a distinct word, not a shade", () => {
    expect(runnerStatePresentation("online")).toEqual({
      label: "Online",
      variant: "good",
    });
    expect(runnerStatePresentation("busy")).toEqual({
      label: "Busy",
      variant: "warning",
    });
    expect(runnerStatePresentation("offline")).toEqual({
      label: "Offline",
      variant: "neutral",
    });
    // Three different words, so the pill is readable with the colour
    // removed entirely — DESIGN.md's rule, and the one 8% of men need.
    const words = ["online", "busy", "offline"].map(
      (s) => runnerStatePresentation(s).label,
    );
    expect(new Set(words).size).toBe(3);
  });

  it("prints a word it has never heard of rather than guessing", () => {
    // A server newer than this bundle. Showing "Online" for a state we
    // did not recognise is how an operator keeps sending work to a
    // machine the server has already given up on.
    const p = runnerStatePresentation("draining");
    expect(p.label).toBe("draining");
    expect(p.variant).toBe("neutral");
  });

  it("does not inherit a presentation from Object.prototype", () => {
    // A plain `Record` lookup on `constructor` returns a function, and
    // `.label` on it is `undefined` — a blank pill. The table's own
    // state column would then be empty for a value that came off the
    // wire, which reads as a rendering bug rather than as a state.
    const p = runnerStatePresentation("constructor");
    expect(p.label).toBe("constructor");
    expect(p.variant).toBe("neutral");
  });
});

describe("orderedLabels", () => {
  it("puts self-hosted, the OS and the arch first, in that order", () => {
    expect(
      orderedLabels(["self-hosted", "linux", "x64", "gpu"], "linux", "x64"),
    ).toEqual(["self-hosted", "linux", "x64", "gpu"]);
  });

  it("is the same list however the server ordered it", () => {
    // The assertion this helper exists for: a runner that re-registers
    // must not reshuffle its chips, or the table reads as two machines.
    expect(
      orderedLabels(["gpu", "x64", "self-hosted", "linux"], "linux", "x64"),
    ).toEqual(["self-hosted", "linux", "x64", "gpu"]);
  });

  it("keeps the custom labels in the order they arrived", () => {
    expect(
      orderedLabels(
        ["self-hosted", "linux", "arm64", "gpu", "cuda"],
        "linux",
        "arm64",
      ),
    ).toEqual(["self-hosted", "linux", "arm64", "gpu", "cuda"]);
  });

  it("dedupes a custom label that repeats what the server added", () => {
    // `--labels linux,gpu` on a linux host: the server adds `linux`
    // too, and two identical chips side by side read as a bug.
    expect(
      orderedLabels(
        ["self-hosted", "linux", "x64", "linux", "gpu"],
        "linux",
        "x64",
      ),
    ).toEqual(["self-hosted", "linux", "x64", "gpu"]);
  });

  it("never invents a label the runner does not carry", () => {
    // Routing is `job.labels ⊆ runner.labels`. A chip for an OS the
    // server did not put on the runner would show a match that the
    // matcher will refuse.
    expect(orderedLabels(["self-hosted", "gpu"], "linux", "x64")).toEqual([
      "self-hosted",
      "gpu",
    ]);
  });

  it("works with no OS or arch to hand", () => {
    expect(orderedLabels(["gpu", "self-hosted"])).toEqual([
      "self-hosted",
      "gpu",
    ]);
    expect(orderedLabels([])).toEqual([]);
  });
});

describe("registrationCommands", () => {
  it("builds both commands, with the browser's own origin", () => {
    expect(registrationCommands("https://stratum.test", "weftg_abc123")).toBe(
      "weft-runner register --url https://stratum.test --token weftg_abc123\n" +
        "weft-runner run",
    );
  });

  it("strips a trailing slash rather than emitting one", () => {
    // `--url https://stratum.test/` works and looks like a typo, which
    // is enough for somebody to retype it wrong.
    expect(registrationCommands("https://stratum.test/", "weftg_x")).toContain(
      "--url https://stratum.test --token weftg_x",
    );
    expect(registrationCommands("https://stratum.test///", "weftg_x")).toContain(
      "--url https://stratum.test ",
    );
  });

  it("keeps a port and a non-root origin intact", () => {
    expect(registrationCommands("http://127.0.0.1:8080", "weftg_x")).toContain(
      "--url http://127.0.0.1:8080 ",
    );
  });

  it("says how long the token lives as a duration, not a clock time", () => {
    // The operator is about to walk to another machine.
    expect(TOKEN_EXPIRY_NOTE).toBe("This token expires in 60 minutes.");
  });
});

describe("groupAccessLine", () => {
  it("says every repository rather than printing the mode", () => {
    expect(groupAccessLine({ repo_access: "all", repos: [] })).toBe(
      "Every repository",
    );
  });

  it("counts the selected repositories, singular and plural", () => {
    expect(
      groupAccessLine({ repo_access: "selected", repos: ["widget"] }),
    ).toBe("1 repository");
    expect(
      groupAccessLine({
        repo_access: "selected",
        repos: ["widget", "session-1"],
      }),
    ).toBe("2 repositories");
    // `selected` with nothing selected admits nothing at all, and the
    // line has to say so rather than reading like a default.
    expect(groupAccessLine({ repo_access: "selected", repos: [] })).toBe(
      "0 repositories",
    );
  });
});

describe("runnerJobHref", () => {
  it("links to the run when the job names its repository", () => {
    expect(
      runnerJobHref("acme", {
        run_id: "wr1",
        job_id: "build",
        key: "build (linux)",
        repo: "widget",
      }),
    ).toBe("/acme/widget/checks/runs/wr1");
  });

  it("returns null rather than a path with a hole in it", () => {
    // The contract's `job` object does not carry the repository, and a
    // link to `/acme/undefined/checks/runs/wr1` is worse than plain
    // text because it looks like it works.
    expect(
      runnerJobHref("acme", { run_id: "wr1", job_id: "build", key: "build" }),
    ).toBeNull();
  });

  it("encodes every segment it is given", () => {
    expect(
      runnerJobHref("ac me", {
        run_id: "wr/1",
        job_id: "b",
        key: "b",
        repo: "wid get",
      }),
    ).toBe("/ac%20me/wid%20get/checks/runs/wr%2F1");
  });
});
