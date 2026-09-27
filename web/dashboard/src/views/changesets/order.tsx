// The landing order, as a picture somebody can correct.
//
// The plan a changeset lands by is a DAG over its members, and it used
// to be rendered as one line of text — "Order: a → b, c → d" — which is
// the data and not the plan: nobody reads three edges and sees two
// waves. This section draws the waves the lander walks, and lets a
// writer edit the edges that make them, through `PUT …/edges`.
//
// Two things it deliberately does not do, both because the alternative
// carries less information than what is here:
//
//   * **No SVG edge splines.** At the sixteen-member maximum, crossing
//     unlabelled curves say less than the `after:` sentence a chip
//     carries when its column is not the whole story; they have no
//     accessible name for the walkthrough or a Playwright assertion to
//     read; and the design system gives dark mode no line vocabulary
//     beyond the hairline, so a spline would be the first drawn stroke
//     on the product.
//   * **No drag-and-drop reordering.** Dragging implies a total order,
//     and the model is a partial one — a DAG, with ties broken by the
//     position a member was added at. A drag also needs a keyboard path
//     of its own, and the manual browser pass cannot drive one, so the
//     gate that exists to look at this screen could not check it. The
//     edge-shaped form below is the wire shape: `{from, to}[]`.

import { useState } from "react";
import {
  api,
  ApiError,
  type Changeset,
  type ChangesetRef,
  type ChangesetVerdict,
  type Session,
} from "@/api";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { cn } from "@/lib/utils";
import { STRUCTURAL_LINK } from "@/lib/links";
import {
  type ChangesetEdge,
  afterLabels,
  declaredEdges,
  edgeError,
  landRefusal,
  ranks,
  refLabel,
  stateGlyph,
  verdictWord,
} from "@/lib/changesets";
import { LandBlockers } from "@/views/forge/change-checks";
import { MountAnchor, type ChangesetLinks } from "@/views/changesets/links";

/// More than four waves is a picture wider than the column it sits in,
/// so it becomes a list of waves instead. The same happens below `md`
/// whatever the count, where there is no room for two columns.
const WIDE_ENOUGH_FOR_COLUMNS = 4;

export function LandingOrder(props: {
  session: Session;
  /// Where a member chip links, in the terms of the mount this strip is
  /// drawn on — the dashboard's, or the forge's public changeset page.
  links: ChangesetLinks;
  cs: Changeset;
  /// Members in landing order, as the members table has them, so the
  /// strip and the table cannot disagree about who is in the set.
  members: Changeset["members"];
  verdict: ChangesetVerdict | null;
  canWrite: boolean;
  busy: boolean;
  /// Re-read the changeset: the server answers `PUT …/edges` with the
  /// new order, and everything else on the page reads it too.
  onSaved: () => void;
}) {
  const { cs, session, members } = props;
  const saved = declaredEdges(cs.edges);
  const dropped = cs.edges.length - saved.length;
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState<ChangesetEdge[]>(saved);
  const [saving, setSaving] = useState(false);
  const [refusal, setRefusal] = useState<string[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refs: ChangesetRef[] = members.map((m) => ({
    repo: m.repo,
    change: m.change.key,
  }));
  // While editing, the strip is drawn from the draft: the point of an
  // editor beside a picture is seeing the plan before saving it.
  const edges = editing ? draft : saved;
  const waves = ranks(refs, edges);
  const problem = editing ? edgeError(refs, draft) : null;
  const stacked = waves.length > WIDE_ENOUGH_FOR_COLUMNS;

  function open() {
    setDraft(saved);
    setRefusal(null);
    setError(null);
    setEditing(true);
  }

  async function save() {
    setSaving(true);
    setRefusal(null);
    setError(null);
    try {
      await api.setChangesetEdges(session, cs.key, draft);
      setEditing(false);
      props.onSaved();
    } catch (e) {
      // A 409 is the gate speaking — the set stopped being open under
      // somebody's hands — and it is shown in the server's words, the
      // same way a refused landing is. An error toast would put the
      // answer somewhere the form is not.
      if (e instanceof ApiError && e.status === 409) {
        const r = landRefusal(e.body);
        setRefusal([r.message, ...r.waitingOn]);
      } else {
        setError(
          `Could not save the order: ${e instanceof Error ? e.message : e}`,
        );
      }
    } finally {
      setSaving(false);
    }
  }

  return (
    <section className="rounded-lg border border-borderline bg-surface-1 p-4">
      <div className="mb-2 flex flex-wrap items-center justify-between gap-2">
        <h2 className="text-sm font-medium text-ink">
          Landing order{" "}
          <span className="font-mono text-xs text-ink-3">
            {waves.length} {waves.length === 1 ? "wave" : "waves"}
          </span>
        </h2>
        {cs.state === "open" &&
          (props.canWrite ? (
            <Button
              type="button"
              variant="outline"
              size="xs"
              disabled={props.busy || saving}
              onClick={() => (editing ? setEditing(false) : open())}
            >
              {editing ? "Cancel" : "Edit order"}
            </Button>
          ) : (
            // The same subtractive rule the actions rail follows: a
            // reader is told what editing takes rather than handed a
            // control that answers `no changeset` when pressed.
            <span className="text-xs text-ink-3">
              Changing the landing order takes write access to every member
              repository.
            </span>
          ))}
      </div>

      {edges.length === 0 ? (
        <p className="text-xs text-ink-3">
          No declared dependencies — these land in the order they were added.
          {cs.state === "open" && props.canWrite
            ? " Edit order to say which must land before which."
            : ""}
        </p>
      ) : null}

      <div
        className={cn(
          "mt-3 flex gap-3",
          // `overflow-x-auto` inside the card, never a negative margin:
          // the walkthrough audits `documentElement.scrollWidth`, and a
          // strip that widens the page fails it.
          stacked ? "flex-col" : "flex-col md:flex-row md:overflow-x-auto",
        )}
      >
        {waves.map((wave, i) => (
          <div
            key={i}
            className={cn(
              "min-w-0 space-y-2",
              stacked ? "" : "md:w-56 md:shrink-0",
            )}
          >
            <div className="font-mono text-[11px] uppercase tracking-widest text-ink-3">
              {i === 0 ? "lands first" : `then (wave ${i + 1})`}
            </div>
            {wave.map((ref) => {
              const m = members.find(
                (x) => x.repo === ref.repo && x.change.key === ref.change,
              );
              const mv = props.verdict?.members.find(
                (v) => v.repo === ref.repo && v.change === ref.change,
              );
              const state = m?.change.state ?? cs.state;
              const after = afterLabels(ref, edges, waves);
              return (
                <div
                  key={refLabel(ref)}
                  className="rounded-lg border border-borderline bg-surface-2 p-2"
                >
                  <MountAnchor
                    link={props.links.member(ref.repo, ref.change)}
                    className={cn("font-mono text-xs", STRUCTURAL_LINK)}
                  >
                    {ref.repo}
                  </MountAnchor>
                  <div className="truncate font-mono text-[11px] text-ink-3">
                    {ref.change}
                  </div>
                  {mv && (
                    <div className="mt-0.5 text-[11px]">
                      <span aria-hidden>{stateGlyph(state)}</span>{" "}
                      <span
                        className={cn(
                          "font-medium",
                          verdictWord(state, mv).tone,
                        )}
                      >
                        {verdictWord(state, mv).word}
                      </span>
                    </div>
                  )}
                  {after.length > 0 && (
                    <div className="mt-0.5 break-words font-mono text-[11px] text-ink-3">
                      after: {after.join(", ")}
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        ))}
      </div>

      {dropped > 0 && (
        <p className="mt-2 text-xs text-warning" role="status">
          {dropped} declared{" "}
          {dropped === 1 ? "dependency names" : "dependencies name"} a change
          this changeset no longer has, and {dropped === 1 ? "is" : "are"} not
          drawn.
        </p>
      )}

      {editing && (
        <div className="mt-4 space-y-2 border-t border-borderline pt-3">
          <p className="text-xs text-ink-3">
            One row per dependency. The picture above follows what you type;
            nothing is written until you save.
          </p>
          {draft.map((e, i) => (
            <div key={i} className="flex flex-wrap items-center gap-2">
              <EdgeEnd
                label={`Dependency ${i + 1}: lands first`}
                value={e.from}
                refs={refs}
                onPick={(r) =>
                  setDraft((d) =>
                    d.map((x, j) => (i === j ? { ...x, from: r } : x)),
                  )
                }
              />
              <span aria-hidden className="text-ink-3">
                →
              </span>
              <EdgeEnd
                label={`Dependency ${i + 1}: lands after`}
                value={e.to}
                refs={refs}
                onPick={(r) =>
                  setDraft((d) =>
                    d.map((x, j) => (i === j ? { ...x, to: r } : x)),
                  )
                }
              />
              <button
                type="button"
                className={cn("text-xs", STRUCTURAL_LINK)}
                aria-label={`Remove dependency ${i + 1}`}
                onClick={() => setDraft((d) => d.filter((_, j) => j !== i))}
              >
                Remove
              </button>
            </div>
          ))}
          <Button
            type="button"
            variant="outline"
            size="xs"
            disabled={refs.length < 2}
            onClick={() =>
              setDraft((d) => [...d, { from: refs[0], to: refs[1] }])
            }
          >
            Add dependency
          </Button>
          {refs.length < 2 && (
            <p className="text-xs text-ink-3">
              A dependency takes two members; this changeset has one.
            </p>
          )}
          {problem && (
            <p className="text-xs text-serious" role="alert">
              {problem}
            </p>
          )}
          <div className="flex flex-wrap items-center gap-2">
            <Button
              type="button"
              size="xs"
              disabled={saving || props.busy || problem !== null}
              onClick={save}
            >
              {saving ? "Saving…" : "Save order"}
            </Button>
            <Button
              type="button"
              variant="outline"
              size="xs"
              disabled={saving}
              onClick={() => setEditing(false)}
            >
              Cancel
            </Button>
          </div>
          {refusal && (
            <div role="alert" className="text-xs text-ink-2">
              <p className="break-words">{refusal[0]}</p>
              {refusal.length > 1 && (
                <div className="mt-1">
                  <LandBlockers blockers={refusal.slice(1)} />
                </div>
              )}
            </div>
          )}
          {error && (
            <p className="text-xs text-serious" role="alert">
              {error}
            </p>
          )}
        </div>
      )}
    </section>
  );
}

/// One end of one edge. Only current members are on offer, because
/// `repo/change` is how the wire names a member and a name the changeset
/// does not have is a refusal the server would have to make.
function EdgeEnd(props: {
  label: string;
  value: ChangesetRef;
  refs: ChangesetRef[];
  onPick: (r: ChangesetRef) => void;
}) {
  return (
    <Select
      value={refLabel(props.value)}
      onValueChange={(v) => {
        const picked = props.refs.find((r) => refLabel(r) === v);
        if (picked) props.onPick(picked);
      }}
    >
      <SelectTrigger aria-label={props.label} className="w-56">
        <SelectValue />
      </SelectTrigger>
      <SelectContent>
        {props.refs.map((r) => (
          <SelectItem key={refLabel(r)} value={refLabel(r)}>
            {refLabel(r)}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}
