import type { ReactNode } from "react";
import { PROSE_LINK } from "@/lib/links";

/// The forge's 404.
///
/// It says which address failed, because on a public forge the common
/// cause is a typo in a name somebody was given verbally, and "we
/// couldn't find `acme/widgit`" is repairable where "Not found" is not.
///
/// It is deliberately the same page for *absent* and *not visible to
/// you*. A private repository that answered 403 would confirm its
/// existence to anybody who guessed the name, which is the enumeration
/// leak the server's masking exists to prevent; the UI must not undo it
/// by rendering a different page for the two cases.
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
            It may have been renamed or deleted — or it may be private, in which
            case signing in with an account that can see it will bring it back.
          </>
        )}
      </p>
      <p className="mt-6 text-sm">
        <a
          href="/"
          onClick={(e) => {
            e.preventDefault();
            props.onNavigate("/");
          }}
          className={PROSE_LINK}
        >
          Go to the front page
        </a>
      </p>
    </div>
  );
}
