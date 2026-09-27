import { useState } from "react";
import { Check, Code2, Copy } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import { cn } from "@/lib/utils";

/// The clone address, where somebody looks for it.
///
/// This replaced a "Clone" card in the About sidebar that stacked both
/// URLs and truncated each to about fifteen characters — the one string
/// on the page a visitor is there to read, cut in half, twice, in the
/// narrowest column available.
///
/// Three things make GitHub's better and all three are copied on
/// purpose. It is a **primary button in the action row**, beside the
/// branch selector, which is where a hand goes looking. It shows **one
/// transport at a time** behind a segmented control, so the URL gets the
/// full width of the panel and can simply be read. And copying is one
/// button next to the thing being copied, rather than a row of controls
/// competing for a narrow column.
///
/// What is deliberately absent: GitHub's "Open with Desktop", "Download
/// ZIP" and Codespaces. We have no desktop app and no archive endpoint,
/// and a menu item that leads nowhere is worse than its absence.
export function CodeButton(props: {
  httpsUrl: string;
  sshUrl: string | null;
  className?: string;
}) {
  const transports = [
    { key: "https" as const, label: "HTTPS", url: props.httpsUrl },
    ...(props.sshUrl
      ? [{ key: "ssh" as const, label: "SSH", url: props.sshUrl }]
      : []),
  ];
  const [pick, setPick] = useState<"https" | "ssh">("https");
  const [copied, setCopied] = useState(false);
  const current = transports.find((t) => t.key === pick) ?? transports[0];

  async function copy() {
    try {
      await navigator.clipboard.writeText(current.url);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      // Clipboard unavailable — over plain http, or refused. The field
      // is selectable, which is the fallback that always works.
    }
  }

  return (
    <Popover>
      <PopoverTrigger asChild>
        <Button size="xs" className={cn("gap-1.5", props.className)}>
          <Code2 aria-hidden className="size-4" />
          Code
        </Button>
      </PopoverTrigger>
      <PopoverContent align="end" className="w-96 p-0">
        <div className="border-b border-borderline px-4 py-3">
          <div className="text-sm font-medium text-ink">Clone</div>
          {transports.length > 1 && (
            <div className="mt-2 inline-flex rounded-lg border border-borderline p-0.5">
              {transports.map((t) => (
                <button
                  key={t.key}
                  onClick={() => setPick(t.key)}
                  aria-pressed={t.key === pick}
                  className={cn(
                    "rounded-md px-3 py-1 text-xs font-medium transition",
                    t.key === pick
                      ? "bg-surface-2 text-ink"
                      : "text-ink-3 hover:text-ink",
                  )}
                >
                  {t.label}
                </button>
              ))}
            </div>
          )}
          <div className="mt-2 flex items-center gap-2">
            <input
              readOnly
              value={current.url}
              aria-label={`${current.label} clone URL`}
              onFocus={(e) => e.currentTarget.select()}
              className="min-w-0 flex-1 rounded-md border border-borderline bg-surface-0 px-2.5 py-1.5 font-mono text-xs text-ink-2"
            />
            <Button
              variant="outline"
              size="icon"
              onClick={copy}
              aria-label={`Copy the ${current.label} clone URL`}
            >
              {copied ? (
                <Check aria-hidden className="size-3.5" />
              ) : (
                <Copy aria-hidden className="size-3.5" />
              )}
            </Button>
          </div>
          <p className="mt-2 text-xs text-ink-3">
            {pick === "https"
              ? "Clone with the web URL. A token works as the password."
              : "Clone with SSH, using a key you have registered."}
          </p>
        </div>
      </PopoverContent>
    </Popover>
  );
}
