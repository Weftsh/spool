import { Button } from "@/components/ui/button";

/// A card, centred, with the product mark. Every screen somebody reaches
/// without a session looks like this one.
export function AuthCard(props: {
  title: string;
  blurb?: string;
  onSubmit: (e: React.FormEvent) => void;
  error?: string | null;
  note?: string | null;
  children?: React.ReactNode;
  footer?: React.ReactNode;
  submit: string;
  busy?: boolean;
}) {
  return (
    <div className="flex min-h-screen items-center justify-center px-5">
      <form
        onSubmit={props.onSubmit}
        className="w-full max-w-sm rounded-xl border border-borderline bg-surface-1 p-6 shadow-[var(--shadow-1)]"
      >
        <h1 className="mb-1 text-lg font-semibold tracking-tight">
          {props.title}
        </h1>
        {props.blurb && (
          <p className="mb-5 text-sm text-ink-3">{props.blurb}</p>
        )}
        {props.children}
        {props.error && (
          <p className="mb-3 text-sm text-serious" role="alert">
            {props.error}
          </p>
        )}
        {props.note && (
          <p className="mb-3 text-sm text-ink-2" role="status">
            {props.note}
          </p>
        )}
        <Button size="lg" className="w-full" disabled={props.busy}>
          {props.busy ? "Working\u2026" : props.submit}
        </Button>
        {props.footer}
      </form>
    </div>
  );
}
