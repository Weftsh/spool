import { useState } from "react";
import { api } from "@/api";
import { Err } from "@/components/feedback";
import { Panel } from "@/components/panel";
import { Button } from "@/components/ui/button";

/// Change your own password. Every other session you hold ends — a
/// password change is what someone does after fearing a compromise.
export function PasswordPanel() {
  const [current, setCurrent] = useState("");
  const [next, setNext] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState(false);
  const [busy, setBusy] = useState(false);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    setDone(false);
    try {
      await api.changePassword(current, next);
      setCurrent("");
      setNext("");
      setDone(true);
    } catch (err) {
      setError(
        `Could not change it: ${err instanceof Error ? err.message : err}`,
      );
    } finally {
      setBusy(false);
    }
  }

  return (
    <Panel
      title="Password"
      hint="Changing it signs out every other session you have open. This one stays."
    >
      <Err message={error} />
      {done && (
        <p className="mb-3 text-sm text-good" role="status">
          Password changed.
        </p>
      )}
      <form onSubmit={submit} className="flex flex-wrap gap-2">
        <input
          className="min-w-0 flex-1 rounded-md border border-borderline bg-surface-0 px-2.5 py-1.5 text-sm"
          type="password"
          autoComplete="current-password"
          placeholder="current password"
          aria-label="Current password"
          value={current}
          onChange={(e) => setCurrent(e.target.value)}
          required
        />
        <input
          className="min-w-0 flex-1 rounded-md border border-borderline bg-surface-0 px-2.5 py-1.5 text-sm"
          type="password"
          autoComplete="new-password"
          placeholder="new password (12+ characters)"
          aria-label="New password"
          value={next}
          onChange={(e) => setNext(e.target.value)}
          required
        />
        <Button disabled={busy}>
          {busy ? "Changing…" : "Change password"}
        </Button>
      </form>
    </Panel>
  );
}
