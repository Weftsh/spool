// An installation that arrived before there was an org to bind it to.
//
// Installing the App from GitHub's side — the Marketplace listing, the
// App's own page — sends the browser to our callback with nothing that
// names an org. The server proves the installation is the person's,
// parks it, and sends them here with `?connect=claim`. This card is the
// missing half: which organization, then one click. It is the only
// screen in the flow that knows the installation exists, so it also has
// to say when the claim has gone (spent, or older than thirty minutes)
// rather than offer a button that would 404.

import { useEffect, useState } from "react";

import { api, type Me, type OrgMembership, type PendingInstall } from "@/api";

/// The organizations a person may connect an installation to: the ones
/// they administer. A member or viewer sees the card but not those orgs,
/// and the server would refuse them anyway; filtering here is so the
/// select does not offer a choice that cannot be made.
export function claimableOrgs(orgs: OrgMembership[]): OrgMembership[] {
  return orgs.filter((o) => o.role === "owner" || o.role === "admin");
}

export function ClaimInstall(props: {
  me: Me | null;
  onConnected: (org: string) => void;
  onCreateOrg: () => void;
}) {
  const [pending, setPending] = useState<PendingInstall | null | "gone">(
    null,
  );
  const [org, setOrg] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const orgs = claimableOrgs(props.me?.orgs ?? []);

  useEffect(() => {
    let alive = true;
    api
      .pendingInstall()
      .then((p) => alive && setPending(p))
      .catch(() => alive && setPending("gone"));
    return () => {
      alive = false;
    };
  }, []);

  useEffect(() => {
    if (!org && orgs.length > 0) setOrg(orgs[0].name);
  }, [org, orgs]);

  if (pending === null) return null;
  if (pending === "gone") {
    return (
      <div
        className="mb-6 rounded-md border border-warning/40 bg-warning/10 px-3 py-2 text-sm"
        role="status"
      >
        There is no GitHub installation waiting to be connected. A claim
        lasts thirty minutes; install the app again from GitHub, or start
        from Connect GitHub in the organization you mean.
      </div>
    );
  }

  const who = pending.account ? `GitHub account ${pending.account}` : "A GitHub installation";

  async function connect(e: React.FormEvent) {
    e.preventDefault();
    if (!org) return;
    setBusy(true);
    setError(null);
    try {
      const out = await api.claimInstall(org);
      props.onConnected(out.org);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <form
      onSubmit={connect}
      className="mb-6 rounded-md border border-accent/40 bg-accent/10 px-3 py-3 text-sm"
      aria-label="Connect the GitHub installation"
    >
      <p className="mb-2">
        <strong>{who}</strong> is ready to connect. Choose the organization
        that will mirror its repositories and run its jobs.
      </p>
      {orgs.length === 0 ? (
        <p>
          You do not administer an organization yet.{" "}
          <button
            type="button"
            className="underline"
            onClick={props.onCreateOrg}
          >
            Create one
          </button>
          , then come back to this page; the installation stays parked for
          thirty minutes.
        </p>
      ) : (
        <div className="flex flex-wrap items-center gap-2">
          <label htmlFor="claim-org" className="sr-only">
            Organization
          </label>
          <select
            id="claim-org"
            className="rounded border border-line bg-surface px-2 py-1"
            value={org}
            onChange={(e) => setOrg(e.target.value)}
          >
            {orgs.map((o) => (
              <option key={o.name} value={o.name}>
                {o.name}
              </option>
            ))}
          </select>
          <button
            type="submit"
            className="rounded bg-accent px-3 py-1 font-medium text-on-accent disabled:opacity-60"
            disabled={busy || !org}
          >
            {busy ? "Connecting…" : "Connect"}
          </button>
          <button
            type="button"
            className="underline"
            onClick={props.onCreateOrg}
          >
            or create an organization
          </button>
        </div>
      )}
      {error && (
        <p role="alert" className="mt-2 text-danger">
          {error}
        </p>
      )}
    </form>
  );
}
