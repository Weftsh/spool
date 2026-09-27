import { describe, expect, it } from "vitest";
import type { GithubInstallation, GithubJob } from "@/api";
import {
  ABANDONED_LINE,
  ACTIONS_WRITE,
  ADMINISTRATION_WRITE,
  STILL_QUEUED_NOTE,
  approveLine,
  installationStatus,
  jobLine,
  readyLine,
  sizeLine,
  snippet,
} from "./github-runners";

function inst(over: Partial<GithubInstallation> = {}): GithubInstallation {
  return {
    installation_id: "4001",
    provider: "github",
    account: "acme-inc",
    created_at: 0,
    detail: {
      account: "acme-inc",
      target_type: "Organization",
      administration_write: true,
      actions_write: true,
      runners_ready: true,
      approve_url:
        "https://github.com/organizations/acme-inc/settings/installations/4001",
      suspended: false,
    },
    ...over,
  };
}

function job(over: Partial<GithubJob> = {}): GithubJob {
  return {
    id: "gj1",
    repo: "acme/pipeline",
    private: true,
    github_job_id: 77,
    github_run_id: 900,
    run_attempt: 1,
    name: "test",
    html_url: "https://github.com/acme/pipeline/actions/runs/900/job/77",
    labels: ["self-hosted", "weft"],
    size: "weft",
    multiplier: 1,
    state: "queued",
    refusal: null,
    cancelled_on_github: false,
    needs_permission: false,
    error: null,
    conclusion: null,
    runner_name: null,
    minutes: 0,
    queued_at: 1,
    launched_at: null,
    started_at: null,
    completed_at: null,
    ...over,
  };
}

describe("installationStatus", () => {
  it("is ready only when both permissions are held", () => {
    expect(installationStatus(inst())).toEqual({ kind: "ready" });
  });

  it("names what is missing, in GitHub's order, with the approve page", () => {
    const d = inst().detail as Exclude<
      GithubInstallation["detail"],
      null | undefined | { gone: true }
    >;
    expect(
      installationStatus(
        inst({
          detail: { ...d, administration_write: false, actions_write: false },
        }),
      ),
    ).toEqual({
      kind: "approve",
      url: d.approve_url,
      missing: [ADMINISTRATION_WRITE, ACTIONS_WRITE],
    });
    expect(
      installationStatus(inst({ detail: { ...d, actions_write: false } })),
    ).toEqual({
      kind: "approve",
      url: d.approve_url,
      missing: [ACTIONS_WRITE],
    });
    expect(
      installationStatus(
        inst({ detail: { ...d, administration_write: false } }),
      ),
    ).toEqual({
      kind: "approve",
      url: d.approve_url,
      missing: [ADMINISTRATION_WRITE],
    });
  });

  it("trusts the two flags over the server's conjunction", () => {
    // `runners_ready` is derived from the flags server-side; if a server
    // ever sent them disagreeing, the flags are what a person is about
    // to go and approve, so they decide.
    const d = inst().detail as Exclude<
      GithubInstallation["detail"],
      null | undefined | { gone: true }
    >;
    expect(
      installationStatus(inst({ detail: { ...d, runners_ready: false } })).kind,
    ).toBe("ready");
  });

  it("says gone when the App was uninstalled on GitHub", () => {
    expect(installationStatus(inst({ detail: { gone: true } }))).toEqual({
      kind: "gone",
    });
  });

  it("never reads 'could not ask' as ready", () => {
    // `null` is GitHub not answering; absent is a server older than the
    // detail. Either way the page has not checked, and a card that says
    // "can use Weft runners" on that basis is the one that gets the
    // first job refused for a permission the page said was there.
    expect(installationStatus(inst({ detail: null }))).toEqual({
      kind: "unknown",
    });
    const older = inst();
    delete older.detail;
    expect(installationStatus(older)).toEqual({ kind: "unknown" });
  });

  it("is none when nothing is connected", () => {
    expect(installationStatus(null)).toEqual({ kind: "none" });
    expect(installationStatus(undefined)).toEqual({ kind: "none" });
  });
});

describe("the card's sentences", () => {
  it("name the GitHub account, not our organisation", () => {
    expect(readyLine("acme-inc")).toBe(
      "GitHub Actions on acme-inc can use Weft runners",
    );
  });

  it("list what to approve, joined with and", () => {
    expect(approveLine([ADMINISTRATION_WRITE, ACTIONS_WRITE])).toBe(
      "Approve Administration: write and Actions: write on GitHub",
    );
    expect(approveLine([ACTIONS_WRITE])).toBe(
      "Approve Actions: write on GitHub",
    );
  });
});

describe("jobLine", () => {
  it("puts the refusal after the state, in the server's words", () => {
    expect(
      jobLine(
        job({
          state: "refused",
          refusal:
            "hosted runners are switched off for this organisation — Settings → Runners",
        }),
      ),
    ).toBe(
      "refused — hosted runners are switched off for this organisation — Settings → Runners",
    );
    expect(jobLine(job({ state: "refused", refusal: null }))).toBe(
      "refused — no reason was recorded",
    );
  });

  it("says what a queued and a launching job are waiting on", () => {
    expect(jobLine(job({ state: "queued" }))).toBe(
      "queued — waiting for a runner",
    );
    expect(jobLine(job({ state: "launching" }))).toBe("launching a runner");
  });

  it("shows minutes while a job is still running", () => {
    // The figure being spent right now, not one revealed once it is
    // over: a person watching a runaway job needs it before the end.
    expect(jobLine(job({ state: "running", minutes: 3 }))).toBe(
      "running · 3 min",
    );
    expect(jobLine(job({ state: "running", minutes: 1200 }))).toBe(
      "running · 1,200 min",
    );
  });

  it("shows the conclusion and the minutes of a completed job", () => {
    expect(
      jobLine(job({ state: "completed", conclusion: "success", minutes: 4 })),
    ).toBe("completed · success · 4 min");
    expect(
      jobLine(job({ state: "completed", conclusion: null, minutes: 4 })),
    ).toBe("completed · no conclusion · 4 min");
  });

  it("gives a failed job its error, then its conclusion, then nothing", () => {
    expect(
      jobLine(
        job({
          state: "failed",
          error: "the runner ran past the fleet's cap and was stopped",
        }),
      ),
    ).toBe("failed — the runner ran past the fleet's cap and was stopped");
    expect(
      jobLine(job({ state: "failed", error: null, conclusion: "failure" })),
    ).toBe("failed — failure");
    expect(jobLine(job({ state: "failed" }))).toBe(
      "failed — no reason was recorded",
    );
  });

  it("explains an abandoned job, and that nothing was billed", () => {
    expect(jobLine(job({ state: "abandoned" }))).toBe(ABANDONED_LINE);
    expect(ABANDONED_LINE).toContain("no job reached this runner");
    expect(ABANDONED_LINE).toContain("nothing was billed");
  });

  it("prints a state it has never heard of rather than guessing", () => {
    expect(jobLine(job({ state: "draining" as GithubJob["state"] }))).toBe(
      "draining",
    );
  });

  it("says what to do about a refused job GitHub still holds", () => {
    expect(STILL_QUEUED_NOTE).toBe("still queued on GitHub — cancel it there");
  });
});

describe("the snippet", () => {
  it("uses the server's label, never a retyped one", () => {
    expect(snippet({ label: "weft" })).toBe("runs-on: weft");
    expect(snippet({ label: "weft-4x" })).toBe("runs-on: weft-4x");
  });

  it("describes a size in vCPU and GB from Fargate units and MiB", () => {
    expect(
      sizeLine({ label: "weft", multiplier: 1, cpu: 1024, memory_mib: 2048 }),
    ).toBe("weft — 1 vCPU, 2 GB, 1× minutes");
    expect(
      sizeLine({
        label: "weft-2x",
        multiplier: 2,
        cpu: 2048,
        memory_mib: 4096,
      }),
    ).toBe("weft-2x — 2 vCPU, 4 GB, 2× minutes");
    expect(
      sizeLine({
        label: "weft-4x",
        multiplier: 4,
        cpu: 4096,
        memory_mib: 8192,
      }),
    ).toBe("weft-4x — 4 vCPU, 8 GB, 4× minutes");
    // A half: shown as a fraction, not rounded away to "0".
    expect(
      sizeLine({ label: "weft-s", multiplier: 1, cpu: 512, memory_mib: 1024 }),
    ).toBe("weft-s — 0.5 vCPU, 1 GB, 1× minutes");
  });
});
