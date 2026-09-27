import type { GithubInstallation, MirrorPush, Repo } from "@/api";

/// What the repository page says about pushing to a mirror, decided
/// here rather than inline so it can be tested without a browser.
///
/// A mirror forwards pushes to its origin. Whether *this* one can is a
/// property of how it was registered — an installation behind it or
/// not, and whether that installation approved `Contents: write` — and
/// the server says which in `repo.push`. The page's job is to say it
/// before the first refused push, in the words the push would be
/// refused with, and to offer the one action that fixes it.
///
/// - `forwarding`: pushes reach the origin.
/// - `approve`: the installation predates the permission; GitHub's
///   page for approving it, and what is missing.
/// - `no-credential`: nothing behind the mirror can push — a pasted
///   public URL; attaching a connected installation fixes it.
/// - `unknown`: a server older than the field. Never rendered as
///   forwarding: a page that says "push here" on the strength of not
///   having asked is how the first push gets refused.
/// - `native`: not a mirror; nothing to say.
export type PushStatus =
  | { kind: "forwarding" }
  | { kind: "approve"; url: string | null; missing: string[] }
  | { kind: "no-credential"; blocked: string }
  | { kind: "unknown" }
  | { kind: "native" };

export const CONTENTS_WRITE = "Contents: write";

export function pushStatus(
  info: Pick<Repo, "kind"> & { push?: MirrorPush | null },
): PushStatus {
  if (info.kind !== "mirror") return { kind: "native" };
  const p = info.push;
  if (p === null || p === undefined) return { kind: "unknown" };
  if (p.forwarding) return { kind: "forwarding" };
  if (p.needs_permission) {
    return { kind: "approve", url: p.approve_url, missing: [CONTENTS_WRITE] };
  }
  return {
    kind: "no-credential",
    blocked: p.blocked ?? "this mirror has no credential that can push to its origin",
  };
}

/// The sentence for the forwarding case. The origin is named because
/// the person is about to add a remote and should know where a push
/// to it ends up.
export function forwardingLine(origin: string | null): string {
  return origin
    ? `Pushes to this mirror are forwarded to ${origin}; GitHub stays canonical.`
    : "Pushes to this mirror are forwarded to its origin; the origin stays canonical.";
}

/// "Approve Contents: write on GitHub" — one permission today, listed
/// the way the runners panel lists its two, so the two sentences read
/// alike side by side.
export function approvePushLine(missing: readonly string[]): string {
  return `Approve ${missing.join(" and ")} on GitHub`;
}

/// Whether an installation in the picker can push, as its row says.
///
/// `push_ready` is the server's reading of `contents_write`; a server
/// older than the field says neither, and that is `unknown` — the
/// picker still lists it, and the mirror made through it says the rest.
export type InstallationPush = "ready" | "approve" | "unknown" | "gone";

export function installationPush(
  inst: GithubInstallation | null | undefined,
): InstallationPush {
  const d = inst?.detail;
  if (d === null || d === undefined) return "unknown";
  if ("gone" in d) return "gone";
  if (d.push_ready === undefined && d.contents_write === undefined) {
    return "unknown";
  }
  return (d.push_ready ?? d.contents_write) ? "ready" : "approve";
}

/// The `git remote add` line under a freshly mirrored repository: the
/// clone URL the server answered with, never one the page assembled.
export function remoteAddLine(cloneUrl: string): string {
  return `git remote add weft ${cloneUrl}`;
}
