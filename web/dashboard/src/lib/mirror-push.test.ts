import { describe, expect, it } from "vitest";
import type { GithubInstallation, MirrorPush } from "@/api";
import {
  CONTENTS_WRITE,
  approvePushLine,
  forwardingLine,
  installationPush,
  pushStatus,
  remoteAddLine,
} from "./mirror-push";

const forwarding: MirrorPush = {
  forwarding: true,
  blocked: null,
  needs_permission: false,
  approve_url: null,
};

function inst(detail: GithubInstallation["detail"]): GithubInstallation {
  return {
    installation_id: "4001",
    provider: "github",
    account: "acme-inc",
    created_at: 0,
    detail,
  };
}

describe("pushStatus", () => {
  it("has nothing to say about a native repository", () => {
    expect(pushStatus({ kind: "native", push: forwarding })).toEqual({
      kind: "native",
    });
  });

  it("says a forwarding mirror forwards", () => {
    expect(pushStatus({ kind: "mirror", push: forwarding })).toEqual({
      kind: "forwarding",
    });
  });

  /// The one case a person fixes on GitHub: the installation predates
  /// the permission. The link is the server's, and what is missing is
  /// named so the sentence can say it.
  it("offers the approve link when the installation lacks the permission", () => {
    const status = pushStatus({
      kind: "mirror",
      push: {
        forwarding: false,
        blocked: "this GitHub App installation cannot push …",
        needs_permission: true,
        approve_url: "https://github.com/settings/installations/4007",
      },
    });
    expect(status).toEqual({
      kind: "approve",
      url: "https://github.com/settings/installations/4007",
      missing: [CONTENTS_WRITE],
    });
  });

  it("carries the server's own sentence for a mirror with no credential", () => {
    const status = pushStatus({
      kind: "mirror",
      push: {
        forwarding: false,
        blocked: "this mirror has no credential that can push to its origin (acme/widget)",
        needs_permission: false,
        approve_url: null,
      },
    });
    expect(status).toEqual({
      kind: "no-credential",
      blocked:
        "this mirror has no credential that can push to its origin (acme/widget)",
    });
  });

  /// A server older than the field says nothing, and nothing is not
  /// "forwarding": the page must not promise a push it has not checked.
  it("never reads an absent field as forwarding", () => {
    expect(pushStatus({ kind: "mirror" })).toEqual({ kind: "unknown" });
    expect(pushStatus({ kind: "mirror", push: null })).toEqual({
      kind: "unknown",
    });
  });
});

describe("the sentences", () => {
  it("name the origin when there is one", () => {
    expect(forwardingLine("github.com/acme/widget")).toBe(
      "Pushes to this mirror are forwarded to github.com/acme/widget; GitHub stays canonical.",
    );
    expect(forwardingLine(null)).toContain("forwarded to its origin");
  });

  it("name the permission to approve, and where", () => {
    expect(approvePushLine([CONTENTS_WRITE])).toBe(
      "Approve Contents: write on GitHub",
    );
  });

  it("build the remote line from the server's clone URL", () => {
    expect(remoteAddLine("https://weft.sh/acme/widget.git")).toBe(
      "git remote add weft https://weft.sh/acme/widget.git",
    );
  });
});

describe("installationPush", () => {
  const detail = {
    account: "acme-inc",
    target_type: "Organization" as const,
    approve_url: "https://github.com/organizations/acme-inc/settings/installations/4001",
    suspended: false,
  };

  it("reads the server's push_ready", () => {
    expect(installationPush(inst({ ...detail, push_ready: true }))).toBe("ready");
    expect(installationPush(inst({ ...detail, push_ready: false }))).toBe(
      "approve",
    );
  });

  /// A detail that says nothing about pushes is a server older than the
  /// field, not a refusal: `unknown`, never `approve`.
  it("does not infer pushes from a detail that is silent about them", () => {
    expect(installationPush(inst(detail))).toBe("unknown");
    expect(installationPush(inst({ ...detail, contents_write: false }))).toBe(
      "approve",
    );
  });

  it("tells gone and unasked apart", () => {
    expect(installationPush(inst({ gone: true }))).toBe("gone");
    expect(installationPush(inst(null))).toBe("unknown");
    expect(installationPush(undefined)).toBe("unknown");
  });
});
