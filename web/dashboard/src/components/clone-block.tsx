import { useState } from "react";

/// Both ways to clone a repo, copy-ready. The SSH row renders only when
/// the deployment exposes an SSH endpoint (ssh_clone_url non-null).
export function CloneBlock(props: { httpsUrl: string; sshUrl: string | null }) {
  return (
    <div className="rounded-lg border border-borderline bg-surface-1 p-4">
      <div className="mb-2 text-sm font-medium text-ink">Clone</div>
      <div className="space-y-2">
        <CopyRow label="HTTPS" value={props.httpsUrl} />
        {props.sshUrl && <CopyRow label="SSH" value={props.sshUrl} />}
      </div>
    </div>
  );
}

function CopyRow(props: { label: string; value: string }) {
  const [copied, setCopied] = useState(false);
  async function copy() {
    try {
      await navigator.clipboard.writeText(props.value);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      /* clipboard unavailable (permissions, http) — the text stays selectable */
    }
  }
  // One row, and the value carried in `title` as well as in the field.
  //
  // This block was written for the dashboard's own column and is reused
  // in the public page's 288px About panel, where the URL does not fit.
  // Letting it wrap put the Copy button on its own line and still cut
  // the text, so the row stays intact: the field selects on focus, Copy
  // takes the whole value whatever is visible, and hovering shows it in
  // full. A truncated string you can copy is fine; a broken layout that
  // still truncates is not.
  return (
    <div className="flex items-center gap-2">
      <span className="w-14 shrink-0 text-xs font-medium uppercase tracking-wider text-ink-3">
        {props.label}
      </span>
      <input
        readOnly
        value={props.value}
        aria-label={`${props.label} clone URL`}
        title={props.value}
        onFocus={(e) => e.currentTarget.select()}
        className="min-w-0 flex-1 rounded-md border border-borderline bg-surface-0 px-2.5 py-1.5 font-mono text-xs text-ink-2"
      />
      <button
        type="button"
        onClick={copy}
        className="shrink-0 rounded-md border border-borderline px-2.5 py-1.5 text-xs font-medium text-ink-2 hover:text-ink"
      >
        {copied ? "Copied" : "Copy"}
      </button>
    </div>
  );
}
