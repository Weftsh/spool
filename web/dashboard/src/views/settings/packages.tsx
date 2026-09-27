import { useCallback, useEffect, useState } from "react";
import {
  api,
  type AdmissionMode,
  type AdmissionPolicy,
  type LicenseMode,
  type Package,
  type PackageDetail,
  type PackageEcosystem,
  type PackageMode,
  type PackagePolicy,
  type PolicyFinding,
  type Session,
} from "@/api";
import { Err, Loading } from "@/components/feedback";
import { Panel } from "@/components/panel";
import { RelativeTime } from "@/components/relative-time";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { dash } from "@/router";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableHeadRow,
  TableRow,
} from "@/components/ui/table";

/// Ecosystems that answer something today. The others are listed by the
/// server and shown here so an admin can see what is coming and so the
/// policy they set now survives its arrival — but switching one on would
/// promise a registry that is not there, so the control is disabled and
/// says why. An enabled-looking toggle that does nothing is worse than
/// an absent one.
const SERVING = new Set(["npm", "maven", "pypi", "cargo", "oci"]);

/// What each mode is called on the screen. A non-admin sees the words
/// and not the control, and the two must agree: a viewer told "Private"
/// about an organization that is in fact proxying from npmjs has been
/// told something false about where its dependencies come from.
const MODE_WORDS: Record<PackageMode, string> = {
  off: "Off",
  private: "Private",
  proxy: "Private + proxy",
};

/// Bytes, as a person reads them.
function size(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i += 1;
  }
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[i]}`;
}

/// The `.npmrc` an organization's developers paste.
///
/// Built from the base the server reported rather than from anything
/// assembled here: the URL shape is the server's business, and a snippet
/// that drifts from it is a snippet that silently stops working.
export function npmrcSnippet(base: string, org: string): string {
  const url = `${base.replace(/\/+$/, "")}/npm/${org}/`;
  const host = url.replace(/^https?:\/\//, "");
  return [
    `@${org}:registry=${url}`,
    `//${host}:_authToken=\${WEFT_TOKEN}`,
  ].join("\n");
}

function ModeControl(props: {
  row: PackageEcosystem;
  busy: boolean;
  onChange: (mode: PackageMode) => void;
}) {
  const { row } = props;
  const serving = SERVING.has(row.ecosystem);
  if (!serving) {
    return (
      <span className="text-sm text-ink-3" data-testid={`mode-${row.ecosystem}`}>
        Not serving yet
      </span>
    );
  }
  return (
    <Select
      value={row.mode}
      onValueChange={(v) => props.onChange(v as PackageMode)}
      disabled={props.busy}
    >
      <SelectTrigger
        className="w-44"
        aria-label={`${row.label} registry mode`}
        data-testid={`mode-${row.ecosystem}`}
      >
        <SelectValue />
      </SelectTrigger>
      <SelectContent>
        <SelectItem value="off">Off</SelectItem>
        <SelectItem value="private">Private</SelectItem>
        <SelectItem value="proxy">Private + proxy</SelectItem>
      </SelectContent>
    </Select>
  );
}

function VersionRows(props: { detail: PackageDetail; onYank: (v: string, yanked: boolean) => void }) {
  return (
    <Table>
      <TableHeader>
        <TableHeadRow>
          <TableHead>Version</TableHead>
          <TableHead>Licence</TableHead>
          <TableHead>Size</TableHead>
          <TableHead>From</TableHead>
          <TableHead>Published</TableHead>
          <TableHead />
        </TableHeadRow>
      </TableHeader>
      <TableBody>
        {props.detail.versions.map((v) => (
          <TableRow key={v.id}>
            <TableCell className="font-mono">
              {v.version}
              {v.yanked && (
                <Badge className="ml-2" variant="warning">
                  yanked
                </Badge>
              )}
            </TableCell>
            <TableCell>
              {v.license ?? <span className="text-ink-3">unknown</span>}
              {v.license && v.license_source !== "declared" && (
                <span className="ml-1 text-xs text-ink-3">({v.license_source})</span>
              )}
            </TableCell>
            <TableCell>{size(v.size_bytes)}</TableCell>
            <TableCell className="font-mono text-xs">
              {v.commit_sha ? (
                v.commit_sha.slice(0, 8)
              ) : (
                <span className="font-sans text-ink-3">published by hand</span>
              )}
            </TableCell>
            <TableCell>
              <RelativeTime at={v.published_at} />
            </TableCell>
            <TableCell className="text-right">
              <Button
                variant="ghost"
                onClick={() => props.onYank(v.version, !v.yanked)}
              >
                {v.yanked ? "Un-yank" : "Yank"}
              </Button>
            </TableCell>
          </TableRow>
        ))}
      </TableBody>
    </Table>
  );
}

/// The admission policy: what may enter this organization's builds from
/// an upstream registry.
///
/// Three controls, kept apart on the screen because they answer three
/// different questions and fail differently. Reserved namespaces decide
/// what may enter *at all*; the cooldown decides what may *change*; the
/// licence rules decide on what terms. Folding them into one "security"
/// toggle is how a person ends up unable to say which of them refused
/// their build.
function PolicyPanel(props: {
  session: Session;
  isAdmin: boolean;
  policy: AdmissionPolicy;
  busy: boolean;
  act: (what: () => Promise<unknown>) => void;
}) {
  const { session, isAdmin, policy, busy, act } = props;
  const [cooldown, setCooldown] = useState(String(policy.cooldown_days));
  const [spdx, setSpdx] = useState("");
  const [pattern, setPattern] = useState("");

  const save = (over: Partial<AdmissionPolicy>) =>
    act(() =>
      api.setPackagePolicy(session, {
        mode: over.mode ?? policy.mode,
        cooldown_days: over.cooldown_days ?? policy.cooldown_days,
        license_mode: over.license_mode ?? policy.license_mode,
      }),
    );

  return (
    <div className="space-y-6">
      <Panel
        title="What happens on a violation"
        hint="Start in audit. It serves the package and writes down what it would have refused, so you can read a week of real findings before anything breaks a build — and then decide."
      >
        <div className="flex items-center gap-3">
          {isAdmin ? (
            <Select
              value={policy.mode}
              onValueChange={(v) => save({ mode: v as AdmissionMode })}
              disabled={busy}
            >
              <SelectTrigger
                className="w-56"
                aria-label="Admission policy mode"
                data-testid="policy-mode"
              >
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="audit">Audit — record and serve</SelectItem>
                <SelectItem value="block">Block — refuse</SelectItem>
              </SelectContent>
            </Select>
          ) : (
            <span className="text-sm" data-testid="policy-mode">
              {policy.mode === "audit" ? "Audit — record and serve" : "Block — refuse"}
            </span>
          )}
          <Badge variant={policy.mode === "block" ? "good" : "neutral"}>
            {policy.mode === "block"
              ? "refusing what the rules below say"
              : "nothing is being refused yet"}
          </Badge>
        </div>
      </Panel>

      <Panel
        title="How long a release must exist before you will take it"
        hint="Nearly every compromised release — event-stream, ua-parser-js, coa, node-ipc — was found and withdrawn within hours to days, and almost nobody needs a package on the day it ships. This is the control that would have caught them. 0 turns it off."
      >
        <div className="flex items-end gap-3">
          <div>
            <label className="text-sm text-ink-2" htmlFor="cooldown">
              Days
            </label>
            <Input
              id="cooldown"
              className="w-28"
              inputMode="numeric"
              value={cooldown}
              disabled={!isAdmin || busy}
              data-testid="cooldown"
              onChange={(e) => setCooldown(e.target.value)}
            />
          </div>
          {isAdmin && (
            <Button
              disabled={busy || cooldown === String(policy.cooldown_days)}
              onClick={() => {
                const n = Number(cooldown);
                if (!Number.isInteger(n)) return;
                save({ cooldown_days: n });
              }}
            >
              Save
            </Button>
          )}
        </div>
        <p className="mt-3 text-sm text-ink-3">
          This is not a vulnerability scan and does not pretend to be one. It
          is a wait, and the wait is what a withdrawal needs to happen in.
        </p>
      </Panel>

      <Panel
        title="Licences"
        hint="Evaluated per version, at the moment metadata is read — packages relicense between versions, so this cannot be decided per package."
      >
        <div className="flex items-center gap-3">
          {isAdmin ? (
            <Select
              value={policy.license_mode}
              onValueChange={(v) => save({ license_mode: v as LicenseMode })}
              disabled={busy}
            >
              <SelectTrigger
                className="w-72"
                aria-label="Licence rule mode"
                data-testid="license-mode"
              >
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="deny_list">
                  Deny list — everything except what is listed
                </SelectItem>
                <SelectItem value="allow_list">
                  Allow list — only what is listed
                </SelectItem>
              </SelectContent>
            </Select>
          ) : (
            <span className="text-sm" data-testid="license-mode">
              {policy.license_mode === "allow_list"
                ? "Allow list — only what is listed"
                : "Deny list — everything except what is listed"}
            </span>
          )}
        </div>

        {policy.license_rules.length === 0 ? (
          <p className="mt-4 text-sm text-ink-3" data-testid="no-license-rules">
            No rules.{" "}
            {policy.license_mode === "deny_list"
              ? "A deny list with no rules admits every licence."
              : "An allow list with no rules admits none, so nothing will resolve until you add one."}
          </p>
        ) : (
          <Table className="mt-4">
            <TableHeader>
              <TableHeadRow>
                <TableHead>Licence</TableHead>
                <TableHead>Rule</TableHead>
                <TableHead />
              </TableHeadRow>
            </TableHeader>
            <TableBody>
              {policy.license_rules.map((r) => (
                <TableRow key={r.spdx_id}>
                  <TableCell className="font-mono uppercase">{r.spdx_id}</TableCell>
                  <TableCell>
                    <Badge variant={r.disposition === "allow" ? "good" : "serious"}>
                      {r.disposition === "allow" ? "allowed" : "denied"}
                    </Badge>
                  </TableCell>
                  <TableCell className="text-right">
                    {isAdmin && (
                      <Button
                        variant="ghost"
                        disabled={busy}
                        onClick={() =>
                          act(() => api.setLicenseRule(session, r.spdx_id, null))
                        }
                      >
                        Remove
                      </Button>
                    )}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        )}

        {isAdmin && (
          <div className="mt-4 flex items-end gap-3">
            <div className="grow">
              <label className="text-sm text-ink-2" htmlFor="spdx">
                SPDX identifier
              </label>
              <Input
                id="spdx"
                placeholder="Apache-2.0"
                value={spdx}
                disabled={busy}
                data-testid="spdx-id"
                onChange={(e) => setSpdx(e.target.value)}
              />
            </div>
            <Button
              disabled={busy || spdx.trim() === ""}
              onClick={() =>
                act(async () => {
                  await api.setLicenseRule(session, spdx.trim(), "allow");
                  setSpdx("");
                })
              }
            >
              Allow
            </Button>
            <Button
              variant="ghost"
              disabled={busy || spdx.trim() === ""}
              onClick={() =>
                act(async () => {
                  await api.setLicenseRule(session, spdx.trim(), "deny");
                  setSpdx("");
                })
              }
            >
              Deny
            </Button>
          </div>
        )}
      </Panel>

      <Panel
        title="Names that are yours"
        hint="A prefix listed here is never fetched from an upstream registry, published or not. Serving what you have published already protects a name you own; this protects one you have not created yet, which is the morning somebody else registers it."
      >
        {policy.reserved.length === 0 ? (
          <p className="text-sm text-ink-3" data-testid="no-reserved">
            Nothing reserved. Claim your scope — <code>@{session.org}</code> —
            before somebody else does.
          </p>
        ) : (
          <ul className="space-y-2" data-testid="reserved">
            {policy.reserved.map((r) => (
              <li key={r} className="flex items-center justify-between">
                <code className="text-sm">{r}</code>
                {isAdmin && (
                  <Button
                    variant="ghost"
                    disabled={busy}
                    onClick={() =>
                      act(() => api.releaseNamespace(session, policy.ecosystem, r))
                    }
                  >
                    Release
                  </Button>
                )}
              </li>
            ))}
          </ul>
        )}
        {isAdmin && (
          <div className="mt-4 flex items-end gap-3">
            <div className="grow">
              <label className="text-sm text-ink-2" htmlFor="reserve">
                Prefix
              </label>
              <Input
                id="reserve"
                placeholder={`@${session.org}`}
                value={pattern}
                disabled={busy}
                data-testid="reserve-pattern"
                onChange={(e) => setPattern(e.target.value)}
              />
            </div>
            <Button
              disabled={busy || pattern.trim() === ""}
              onClick={() =>
                act(async () => {
                  await api.reserveNamespace(
                    session,
                    policy.ecosystem,
                    pattern.trim(),
                  );
                  setPattern("");
                })
              }
            >
              Reserve
            </Button>
          </div>
        )}
        <p className="mt-3 text-sm text-ink-3">
          Matched on a segment boundary. <code>@{session.org}</code> covers{" "}
          <code>@{session.org}/widget</code> and not{" "}
          <code>@{session.org}corp/widget</code>.
        </p>
      </Panel>
    </div>
  );
}

const RULE_WORDS: Record<string, string> = {
  license: "licence",
  cooldown: "too new",
  reserved: "your name",
};

/// What the policy caught.
///
/// The column that matters is the disposition: a screen full of
/// `would_block` is an organization reading what switching to block
/// would cost it, which is the decision this whole feature turns on.
function FindingsPanel(props: {
  session: Session;
  isAdmin: boolean;
  findings: PolicyFinding[];
  busy: boolean;
  act: (what: () => Promise<unknown>) => void;
}) {
  const { session, isAdmin, findings, busy, act } = props;
  const wouldBlock = findings.filter((f) => f.disposition === "would_block").length;

  return (
    <Panel
      title="What the policy caught"
      hint="One row per package version, however many times it came up. The count is what tells you whether a rule is worth keeping."
    >
      {findings.length === 0 ? (
        <p className="text-sm text-ink-3" data-testid="no-findings">
          Nothing yet. Either no upstream package has met a rule, or no
          ecosystem is proxying.
        </p>
      ) : (
        <>
          {wouldBlock > 0 && (
            <p className="mb-4 text-sm text-ink-2" data-testid="would-block-count">
              {wouldBlock === 1
                ? "1 package was served that blocking would have refused."
                : `${wouldBlock} packages were served that blocking would have refused.`}
            </p>
          )}
          <Table>
            <TableHeader>
              <TableHeadRow>
                <TableHead>Package</TableHead>
                <TableHead>Why</TableHead>
                <TableHead>Outcome</TableHead>
                <TableHead>Times</TableHead>
                <TableHead>Last</TableHead>
                <TableHead />
              </TableHeadRow>
            </TableHeader>
            <TableBody>
              {findings.map((f) => (
                <TableRow key={`${f.ecosystem}/${f.name}/${f.version}`}>
                  <TableCell className="font-mono">
                    {f.name}
                    {f.version !== "*" && (
                      <>
                        {/* The space belongs between the name and the
                            tag, not inside it: with it inside, the
                            markup is `left-pad<span> 1.3.0</span>` and
                            anything that strips tags reads one word,
                            `left-pad1.3.0`. */}
                        {" "}
                        <span className="text-ink-3">{f.version}</span>
                      </>
                    )}
                    <div className="mt-1 max-w-xl font-sans text-xs text-ink-3">
                      {f.reason}
                    </div>
                  </TableCell>
                  <TableCell>{RULE_WORDS[f.rule] ?? f.rule}</TableCell>
                  <TableCell>
                    <Badge
                      variant={f.disposition === "blocked" ? "serious" : "warning"}
                    >
                      {f.disposition === "blocked" ? "refused" : "would refuse"}
                    </Badge>
                  </TableCell>
                  <TableCell>{f.hits}</TableCell>
                  <TableCell>
                    <RelativeTime at={f.last_at} />
                  </TableCell>
                  <TableCell className="text-right">
                    {isAdmin && (
                      <Button
                        variant="ghost"
                        disabled={busy}
                        onClick={() =>
                          act(() => api.forgetFinding(session, f))
                        }
                      >
                        Dismiss
                      </Button>
                    )}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
          <p className="mt-3 text-sm text-ink-3">
            Dismissing a row does not admit the package — the rule is still in
            force and the next install writes the row again. To admit it,
            change the rule that refused it.
          </p>
        </>
      )}
    </Panel>
  );
}

/// The registry's settings screen: which ecosystems are on, how to point
/// a client at them, and what has been published.
export function PackagesPanel(props: { session: Session; isAdmin: boolean }) {
  const { session, isAdmin } = props;
  const [policy, setPolicy] = useState<PackagePolicy | null>(null);
  const [admission, setAdmission] = useState<AdmissionPolicy | null>(null);
  const [findings, setFindings] = useState<PolicyFinding[]>([]);
  const [packages, setPackages] = useState<Package[] | null>(null);
  const [open, setOpen] = useState<PackageDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // What a publish here would be told, from the billing view. Read
  // beside the registry rather than inferred from the plan: the server
  // decides, and a screen that guessed would say "publish away" to an
  // organization every publish refuses.
  const [refusal, setRefusal] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    api
      .billing(session)
      .then((b) => live && setRefusal(b.packages_refusal ?? null))
      // No billing view is no refusal to show: the registry's own
      // answers still say it at the moment of publishing.
      .catch(() => live && setRefusal(null));
    return () => {
      live = false;
    };
  }, [session]);

  const load = useCallback(async () => {
    try {
      const [p, list, adm, found] = await Promise.all([
        api.packageEcosystems(session),
        api.packages(session),
        api.packagePolicy(session),
        api.packageFindings(session),
      ]);
      setPolicy(p);
      setPackages(list);
      setAdmission(adm);
      setFindings(found);
      setError(null);
    } catch (e) {
      setError(String(e instanceof Error ? e.message : e));
    }
  }, [session]);

  useEffect(() => {
    void load();
  }, [load]);

  async function act(what: () => Promise<unknown>) {
    setBusy(true);
    try {
      await what();
      await load();
      setError(null);
    } catch (e) {
      setError(String(e instanceof Error ? e.message : e));
    } finally {
      setBusy(false);
    }
  }

  if (!policy || !packages || !admission)
    return error ? <Err message={error} /> : <Loading />;

  const anyOn = policy.ecosystems.some((e) => e.mode !== "off");
  // The policy only decides anything about packages that arrive from
  // somewhere else, so it is shown when something proxies or when there
  // is already a finding to read. Showing it against a purely private
  // registry would be a screenful of controls that cannot change any
  // answer.
  const anyProxy = policy.ecosystems.some((e) => e.mode === "proxy");

  return (
    <div className="space-y-6">
      <Err message={error} />

      {refusal && (
        <div
          className="rounded-lg border border-borderline bg-surface-2 p-4 text-sm"
          data-testid="packages-refusal"
        >
          <p className="font-medium">Publishing is off for this organization.</p>
          <p className="mt-1 text-ink-2">{refusal.replace(/^quota: /, "")}</p>
          <p className="mt-2">
            <a className="text-brand hover:underline" href={dash(["settings", "billing"])}>
              Go to Billing
            </a>
          </p>
        </div>
      )}

      <Panel
        title="Ecosystems"
        hint="Nothing is on until you turn it on. An ecosystem that is off answers nothing at all, which is deliberate: a registry nobody enabled should look like no registry rather than an empty one."
      >
        <Table>
          <TableHeader>
            <TableHeadRow>
              <TableHead>Ecosystem</TableHead>
              <TableHead>Mode</TableHead>
            </TableHeadRow>
          </TableHeader>
          <TableBody>
            {policy.ecosystems.map((row) => (
              <TableRow key={row.ecosystem}>
                <TableCell className="font-medium">{row.label}</TableCell>
                <TableCell>
                  {isAdmin ? (
                    <ModeControl
                      row={row}
                      busy={busy}
                      onChange={(mode) =>
                        void act(() =>
                          api.setPackageEcosystem(session, row.ecosystem, mode),
                        )
                      }
                    />
                  ) : (
                    <span
                      className="text-sm"
                      data-testid={`mode-${row.ecosystem}`}
                    >
                      {MODE_WORDS[row.mode]}
                    </span>
                  )}
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
        {!isAdmin && (
          <p className="mt-3 text-sm text-ink-3">
            Only an owner or admin can change this. It decides what every build
            in the organization may reach.
          </p>
        )}
      </Panel>

      {anyOn && (
        <Panel
          title="Pointing npm at it"
          hint="Put this in .npmrc, with a token minted under Tokens carrying package:read to install or package:write to publish."
        >
          <pre className="overflow-x-auto rounded-lg bg-surface-2 p-4 text-xs leading-relaxed">
            <code data-testid="npmrc">
              {npmrcSnippet(policy.registry_base, session.org)}
            </code>
          </pre>
        </Panel>
      )}

      <Panel
        title="Published"
        hint="Every version records the repository, commit and job that produced it — the question worth being able to answer during an incident."
      >
        {packages.length === 0 ? (
          <p className="text-sm text-ink-3" data-testid="no-packages">
            Nothing published yet.
          </p>
        ) : (
          <Table>
            <TableHeader>
              <TableHeadRow>
                <TableHead>Package</TableHead>
                <TableHead>Ecosystem</TableHead>
                <TableHead>Updated</TableHead>
                <TableHead />
              </TableHeadRow>
            </TableHeader>
            <TableBody>
              {packages.map((p) => (
                <TableRow key={p.id}>
                  <TableCell className="font-mono">{p.name}</TableCell>
                  <TableCell>{p.ecosystem}</TableCell>
                  <TableCell>
                    <RelativeTime at={p.updated_at} />
                  </TableCell>
                  <TableCell className="text-right">
                    <Button
                      variant="ghost"
                      onClick={() =>
                        void act(async () => {
                          setOpen(await api.packageDetail(session, p.id));
                        })
                      }
                    >
                      Versions
                    </Button>
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        )}
      </Panel>

      {(anyProxy || findings.length > 0) && (
        <>
          <PolicyPanel
            session={session}
            isAdmin={isAdmin}
            policy={admission}
            busy={busy}
            act={(w) => void act(w)}
          />
          <FindingsPanel
            session={session}
            isAdmin={isAdmin}
            findings={findings}
            busy={busy}
            act={(w) => void act(w)}
          />
        </>
      )}

      {open && (
        <Panel
          title={open.name}
          hint="A yanked version is hidden from resolution and still downloads by exact version, so a lockfile that already names it keeps building."
        >
          <VersionRows
            detail={open}
            onYank={(version, yanked) =>
              void act(async () => {
                await api.yankPackageVersion(session, open.id, version, yanked);
                setOpen(await api.packageDetail(session, open.id));
              })
            }
          />
        </Panel>
      )}
    </div>
  );
}
