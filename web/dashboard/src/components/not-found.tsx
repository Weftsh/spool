import type { ReactNode } from "react";
import { PROSE_LINK } from "@/lib/links";
import { DASH } from "@/router";

/// The forge's 404.
///
/// It says which address failed, because the common cause is a typo in
/// a name somebody was given verbally, and "we couldn't find
/// `acme/widgit`" is repairable where "Not found" is not.
///
/// It is deliberately the same page for *absent* and *not visible to
/// you*. A repository that answered 403 would confirm its existence to
/// anybody who guessed the name, which is the enumeration leak the
/// server's masking exists to prevent; the UI must not undo it by
/// rendering a different page for the two cases.
export function NotFound(props: {
  /// What was looked for, as the address bar had it —
  /// `acme/widget`, `@ada`.
  what?: string | null;
  /// Overrides the second line where a page has something better to
  /// say, e.g. an issue number that has been deleted.
  children?: ReactNode;
  onNavigate: (to: string) => void;
}) {
  return (
    <div className="mx-auto max-w-lg py-16 text-center">
      <p className="font-mono text-sm text-ink-3">404</p>
      <h1 className="mt-2 text-2xl font-semibold tracking-tight text-ink">
        {props.what ? (
          <>
            We couldn&rsquo;t find{" "}
            <span className="font-mono text-ink-2">{props.what}</span>
          </>
        ) : (
          "We couldn’t find that page"
        )}
      </h1>
      <p className="mt-3 text-sm text-ink-2">
        {props.children ?? (
          <>
            It may have been renamed or deleted — or you may not have access
            to it. Somebody who administers it can grant that.
          </>
        )}
      </p>
      <p className="mt-6 text-sm">
        {/* `onNavigate("/")` on either mount: the dashboard's navigator
            resolves it against `/dashboard`, and the forge's `/` moves
            the browser there. The `href` is the absolute answer, for a
            middle-click. */}
        <a
          href={DASH}
          onClick={(e) => {
            e.preventDefault();
            props.onNavigate("/");
          }}
          className={PROSE_LINK}
        >
          Go to your dashboard
        </a>
      </p>
    </div>
  );
}
