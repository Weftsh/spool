import { useCallback, useEffect, useState } from "react";
import { api, type Me, type Role, type Session } from "@/api";
import { AuthCard } from "@/components/auth-card";
import {
  GithubSigninBanner,
  type GithubSigninOutcome,
} from "@/components/github-signin-banner";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";

/// Redeeming a confirmation or a reset link.
///
/// One component for both because they are one shape: a token out of a
/// mailbox, redeemed once, signing the person in. They differ in what
/// they ask for — a reset needs the new password, a confirmation needs
/// nothing — and in what a dead link means, which is the part worth
/// saying differently.
export function RedeemLink(props: {
  kind: "verify" | "reset";
  token: string;
  onDone: (me: Me) => void;
  onDismissed: () => void;
}) {
  const { kind, token } = props;
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [dead, setDead] = useState(false);
  const [ran, setRan] = useState(false);

  const redeem = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      props.onDone(
        kind === "verify"
          ? await api.verifyEmail(token)
          : await api.resetPassword(token, password),
      );
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      // A dead link is not a retryable error: saying "try again" about a
      // link that can never work again sends somebody round a loop.
      if (/not valid any more/.test(message)) setDead(true);
      else setError(message);
    } finally {
      setBusy(false);
    }
  }, [kind, token, password, props]);

  // A confirmation link has nothing to ask for, so it redeems on
  // arrival: making somebody click "confirm" after clicking a link that
  // said "confirm" is a step with nothing behind it.
  useEffect(() => {
    if (kind === "verify" && !ran) {
      setRan(true);
      void redeem();
    }
  }, [kind, ran, redeem]);

  if (dead) {
    return (
      <AuthCard
        title={kind === "verify" ? "Link expired" : "Reset link expired"}
        blurb={
          kind === "verify"
            ? "This confirmation link is not valid any more \u2014 it may already have been used, or it may have expired. Sign in and ask for another."
            : "This reset link is not valid any more \u2014 it may already have been used, or it may have expired. Ask for a new one from the sign-in screen."
        }
        onSubmit={(e) => {
          e.preventDefault();
          props.onDismissed();
        }}
        submit="Go to sign in"
      />
    );
  }

  if (kind === "verify") {
    return (
      <AuthCard
        title="Confirming your address"
        blurb="One moment."
        onSubmit={(e) => e.preventDefault()}
        error={error}
        submit="Confirming\u2026"
        disabled
      />
    );
  }

  return (
    <AuthCard
      title="Choose a new password"
      blurb="Setting it signs out every other session on the account."
      onSubmit={(e) => {
        e.preventDefault();
        void redeem();
      }}
      error={error}
      submit="Set password"
      busy={busy}
      footer={
        <button
          type="button"
          className="mt-3 w-full text-xs text-ink-3 underline-offset-2 hover:text-ink-2 hover:underline"
          onClick={props.onDismissed}
        >
          Cancel
        </button>
      }
    >
      <Label htmlFor="new-password">New password</Label>
      <Input
        id="new-password"
        className="mb-3"
        value={password}
        onChange={(e) => setPassword(e.target.value)}
        type="password"
        autoComplete="new-password"
        autoFocus
        required
      />
    </AuthCard>
  );
}

/// The screen an emailed invitation link lands on.
///
/// Two shapes, chosen by whether somebody is already signed in here.
/// Signed in: one button, because the account exists and the invitation
/// only has to be attached to it. Not signed in: name and password,
/// because accepting may be creating the account.
///
/// What it deliberately does *not* do is ask the server whether the
/// address already has an account so it can pick the right form. That
/// question is an existence oracle for anyone holding a link, and the
/// same reason sign-in answers identically for a wrong password and an
/// unknown address.
export function AcceptInvite(props: {
  token: string;
  me: Me | null;
  onAccepted: (me: Me) => void;
  onDismissed: () => void;
}) {
  const { token, me } = props;
  const [name, setName] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [invitation, setInvitation] = useState<{
    org: string;
    role: Role;
    email: string;
  } | null>(null);
  const [dead, setDead] = useState(false);

  // Say what this invitation is for before asking for a password.
  // "Accept your invitation" with no organization on the screen asks
  // somebody to hand over a credential in exchange for nothing they can
  // check — and a link that has already been used should say so here,
  // not after they have chosen a password.
  useEffect(() => {
    let alive = true;
    api
      .previewInvite(token)
      .then((i) => alive && setInvitation(i))
      .catch(() => alive && setDead(true));
    return () => {
      alive = false;
    };
  }, [token]);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const who = me
        ? await api.acceptInvite(token, "")
        : await api.acceptInvite(token, name.trim(), password);
      props.onAccepted(who);
    } catch (err) {
      setError(
        `Could not accept this invitation: ${
          err instanceof Error ? err.message : err
        }`,
      );
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="flex min-h-screen items-center justify-center px-5">
      <form
        onSubmit={submit}
        className="w-full max-w-sm rounded-xl border border-borderline bg-surface-1 p-6 shadow-[var(--shadow-1)]"
      >
        <h1 className="mb-1 text-lg font-semibold tracking-tight">
          {invitation ? `Join ${invitation.org}` : "Accept your invitation"}
        </h1>
        <p className="mb-5 text-sm text-ink-3">
          {dead
            ? "This invitation is not valid any more. It may already have been used, or it may have expired — ask whoever invited you for a new one."
            : invitation
              ? `${invitation.email} was invited as ${invitation.role}.${
                  me
                    ? ` Accepting adds ${me.email}, the account you are signed in as.`
                    : " Choose a password and you're in. The link works once."
                }`
              : "Checking your invitation\u2026"}
        </p>
        {!me && !dead && (
          <>
            <Label htmlFor="name">Your name</Label>
            <Input
              id="name"
              className="mb-3"
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="Ada Lovelace"
              autoComplete="name"
              autoFocus
              required
            />
            <Label htmlFor="new-password">Password</Label>
            <Input
              id="new-password"
              className="mb-3"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              type="password"
              autoComplete="new-password"
              required
            />
          </>
        )}
        {error && (
          <p className="mb-3 text-sm text-serious" role="alert">
            {error}
          </p>
        )}
        {!dead && (
          <Button size="lg" className="w-full" disabled={busy || !invitation}>
            {busy ? "Accepting\u2026" : "Accept invitation"}
          </Button>
        )}
        <button
          type="button"
          className="mt-3 w-full text-xs text-ink-3 underline-offset-2 hover:text-ink-2 hover:underline"
          onClick={props.onDismissed}
        >
          {dead
            ? "Go to sign in"
            : me
              ? "Not now"
              : "I already have an account \u2014 sign in instead"}
        </button>
      </form>
    </div>
  );
}

/// What each mode of the sign-in screen is for, in its own words: the
/// heading, and one line under it. Sign-up's line is the reason to finish
/// the form, said once more at the moment somebody decides whether to.
const AUTH_HEADINGS: Record<
  "person" | "token" | "signup" | "forgot",
  [string, string]
> = {
  person: ["Sign in to Weft", "Welcome back."],
  signup: [
    "Create your Weft account",
    "Free for public work. No card needed.",
  ],
  forgot: [
    "Reset your password",
    "We will email you a link to choose a new one.",
  ],
  token: [
    "Sign in with a token",
    "For scripts, CI and service accounts.",
  ],
};

/// Sign in as a person, or — for service callers and for anyone whose
/// account has not been created yet — with a raw API token.
///
/// The person path is the default because pasting an org-wide token into
/// a browser hands a long-lived credential to every script on the page;
/// a session cookie is HttpOnly and cannot be read at all.
export function Login(props: {
  onSignedIn: (s: Session, me: Me | null) => void;
  /// Which form to open on. `/login?mode=signup` is where the site's
  /// "Sign up free" lands, and a button that says sign up must not open
  /// a form that says sign in. Absent means sign-in, the default above.
  initialMode?: "person" | "signup";
  /// How a trip through GitHub came back, when it came back refused.
  ///
  /// Every one of those redirects lands *here*, signed out, so this is
  /// the only screen that can say what happened. The signed-in shell's
  /// banner never sees them.
  githubOutcome?: GithubSigninOutcome | null;
  /// A sentence above the form saying why the person is here, when the
  /// page they came from knows: an install that began on GitHub lands
  /// signed-out visitors here with an installation waiting.
  notice?: string;
}) {
  // `sent` is the screen after a sign-up: the form is gone, because a
  // form that stays put with its button still lit reads as "it did not
  // work, try again" — and trying again is what the server rate-limits.
  // What is left is the one thing to do (read the mail), the way to ask
  // for the message again, and the way in.
  const [mode, setMode] = useState<
    "person" | "token" | "signup" | "forgot" | "sent"
  >(props.initialMode ?? "person");
  const [name, setName] = useState("");
  const [handle, setHandle] = useState("");
  const [note, setNote] = useState<string | null>(null);
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [org, setOrg] = useState("");
  const [token, setToken] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    setNote(null);
    try {
      if (mode === "signup") {
        // The answer is deliberately the same whether or not the address
        // was already registered, so this says the same thing too. It
        // must not read as "your account is ready" — nothing is, until
        // the link in the message is clicked.
        await api.signup({
          email: email.trim(),
          name: name.trim(),
          password,
          handle: handle.trim(),
        });
        setNote(
          `If ${email.trim()} can receive mail here, a confirmation link is on its way. Click it to finish setting up your account.`,
        );
        setMode("sent");
        return;
      }
      if (mode === "forgot") {
        await api.forgotPassword(email.trim());
        setNote(
          `If ${email.trim()} has an account here, a reset link is on its way. It works once and expires in an hour.`,
        );
        return;
      }
      if (mode === "person") {
        const who = await api.login(email.trim(), password);
        if (who.orgs.length === 0) {
          setError("This account is not a member of any organization yet.");
          return;
        }
        props.onSignedIn({ org: who.orgs[0].name, token: "" }, who);
      } else {
        const session = { org: org.trim(), token: token.trim() };
        await api.repos(session);
        props.onSignedIn(session, null);
      }
    } catch (err) {
      const what =
        mode === "signup"
          ? "Could not create that account"
          : mode === "forgot"
            ? "Could not send a reset link"
            : "Could not sign in";
      setError(`${what}: ${err instanceof Error ? err.message : err}`);
    } finally {
      setBusy(false);
    }
  }

  /// The message again, for an inbox that has nothing. Same answer
  /// either way, for the same reason sign-up's is.
  async function resend() {
    setBusy(true);
    setError(null);
    try {
      await api.resendVerification(email.trim());
      setNote(
        `Another confirmation link is on its way to ${email.trim()}, if it can receive mail here. Check the spam folder too — the message comes from no-reply@weft.sh.`,
      );
    } catch (err) {
      setError(
        `Could not send the message again: ${err instanceof Error ? err.message : err}`,
      );
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="flex min-h-screen flex-col items-center justify-center gap-4 px-5">
      {props.notice && (
        <p
          role="status"
          className="w-full max-w-sm rounded-md border border-accent/40 bg-accent/10 px-3 py-2 text-sm"
        >
          {props.notice}
        </p>
      )}
      <form
        onSubmit={submit}
        className="w-full max-w-sm rounded-xl border border-borderline bg-surface-1 p-6 shadow-[var(--shadow-1)]"
      >
        {/* The mark goes home: somebody who clicked "Sign up" on the
            marketing site and wants to read one more page had no way back
            but the browser's button. The heading says what this screen is
            for — it used to read "Weft Dashboard" on every mode, sign-up
            included, and was a <span>, so the page had no heading at all. */}
        <a
          href="/"
          className="mb-5 inline-flex items-center gap-2 rounded-sm text-sm font-semibold tracking-tight text-ink focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-brand"
        >
          <svg width="20" height="20" viewBox="0 0 32 32" aria-hidden>
            <rect
              x="4"
              y="6"
              width="24"
              height="5"
              rx="2"
              fill="var(--brand)"
            />
            <rect
              x="4"
              y="14"
              width="24"
              height="5"
              rx="2"
              fill="var(--accent)"
            />
            <rect
              x="4"
              y="22"
              width="24"
              height="5"
              rx="2"
              fill="var(--series-3)"
            />
          </svg>
          Weft
        </a>
        {mode !== "sent" && (
          <div className="mb-5">
            <h1 className="text-xl font-semibold tracking-tight">
              {AUTH_HEADINGS[mode][0]}
            </h1>
            <p className="mt-1 text-sm text-ink-3">{AUTH_HEADINGS[mode][1]}</p>
          </div>
        )}
        {mode === "sent" && (
          <div className="mb-3">
            <h1 className="text-xl font-semibold tracking-tight">
              Check your email
            </h1>
            <p className="mt-1 text-sm text-ink-3">
              The account is not finished until the link in the message is
              clicked. Nothing else to do here.
            </p>
          </div>
        )}
        {props.githubOutcome && (
          <GithubSigninBanner outcome={props.githubOutcome} />
        )}
        {/* The fast way in, above the form rather than below it: GitHub
            has already proved the address, so this path skips the
            confirmation mail entirely and lands on picking repositories
            to mirror. A full-page link, not a fetch — the flow leaves
            for GitHub and comes back to our callback, and an anchor
            works with no script running. */}
        {(mode === "person" || mode === "signup") && (
          <>
            <Button asChild size="lg" variant="outline" className="w-full">
              <a href="/v1/auth/github/start">
                <svg
                  width="16"
                  height="16"
                  viewBox="0 0 16 16"
                  fill="currentColor"
                  aria-hidden
                >
                  <path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82a7.4 7.4 0 0 1 2-.27c.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.01 8.01 0 0 0 16 8c0-4.42-3.58-8-8-8Z" />
                </svg>
                Continue with GitHub
              </a>
            </Button>
            {/* The rules sit against the word, so the word gets an
                element of its own: bare text touching an inline tag
                reads as one run to anything that strips markup, which
                is what the walkthrough's layout audit reports. */}
            <div className="my-4 flex items-center gap-3 text-xs text-ink-3">
              <span className="h-px flex-1 bg-borderline" aria-hidden />
              <span>or</span>
              <span className="h-px flex-1 bg-borderline" aria-hidden />
            </div>
          </>
        )}
        {mode === "signup" && (
          <>
            <Label htmlFor="name">Your name</Label>
            <Input
              id="name"
              className="mb-3"
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="Ada Lovelace"
              autoComplete="name"
              autoFocus
              required
            />
            <Label htmlFor="handle">Namespace</Label>
            <Input
              id="handle"
              className="mb-3 font-mono"
              value={handle}
              onChange={(e) => setHandle(e.target.value)}
              placeholder="ada"
              required
            />
            <p className="-mt-2 mb-3 text-xs text-ink-3">
              Your repositories will live at{" "}
              <code className="font-mono">/{handle.trim() || "you"}/repo</code>.
              Pick carefully — it goes in every clone URL you hand out.
            </p>
          </>
        )}
        {mode === "person" || mode === "signup" || mode === "forgot" ? (
          <>
            <Label htmlFor="email">Email</Label>
            <Input
              id="email"
              className="mb-3"
              value={email}
              onChange={(e) => setEmail(e.target.value)}
              placeholder="you@example.com"
              type="email"
              autoComplete="username"
              // First field of sign-in and reset, third of sign-up. Two
              // autoFocus props on one form and the later one wins, so
              // sign-up used to open with the cursor in Email, below the
              // name and namespace it had not asked for yet.
              autoFocus={mode !== "signup"}
              required
            />
            {mode !== "forgot" && (
              <>
                <Label htmlFor="password">Password</Label>
                <Input
                  id="password"
                  className="mb-3"
                  value={password}
                  onChange={(e) => setPassword(e.target.value)}
                  type="password"
                  autoComplete={
                    mode === "signup" ? "new-password" : "current-password"
                  }
                  required
                />
              </>
            )}
          </>
        ) : mode === "token" ? (
          <>
            <Label htmlFor="org">Organization</Label>
            <Input
              id="org"
              className="mb-3"
              value={org}
              onChange={(e) => setOrg(e.target.value)}
              placeholder="acme"
              autoFocus
              required
            />
            <Label htmlFor="token">API token</Label>
            <Input
              id="token"
              className="mb-3 font-mono"
              value={token}
              onChange={(e) => setToken(e.target.value)}
              placeholder="weft_…"
              type="password"
              required
            />
          </>
        ) : null}
        {error && (
          <p className="mb-3 text-sm text-serious" role="alert">
            {error}
          </p>
        )}
        {note && (
          <p className="mb-3 text-sm text-ink-2" role="status">
            {note}
          </p>
        )}
        {mode === "sent" ? (
          <Button
            type="button"
            size="lg"
            variant="outline"
            className="w-full"
            disabled={busy}
            onClick={() => void resend()}
          >
            {busy ? "Sending…" : "Send the message again"}
          </Button>
        ) : (
          <Button size="lg" className="w-full" disabled={busy}>
            {busy
              ? "Checking…"
              : mode === "signup"
                ? "Create account"
                : mode === "forgot"
                  ? "Send a reset link"
                  : "Sign in"}
          </Button>
        )}
        {[
          mode === "person"
            ? { to: "signup" as const, label: "Create an account" }
            : { to: "person" as const, label: "Sign in instead" },
          mode === "person"
            ? { to: "forgot" as const, label: "Forgot your password?" }
            : null,
          mode === "person"
            ? {
                to: "token" as const,
                label: "Sign in with an API token instead",
              }
            : mode === "token"
              ? {
                  to: "person" as const,
                  label: "Sign in with email and password",
                }
              : null,
        ]
          .filter((x) => x !== null)
          .map((x) => (
            <button
              key={x.label}
              type="button"
              className="mt-3 w-full rounded-sm text-sm text-ink-2 underline-offset-2 hover:text-ink hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-brand"
              onClick={() => {
                setMode(x.to);
                setError(null);
                setNote(null);
              }}
            >
              {x.label}
            </button>
          ))}
        <p className="mt-3 text-xs text-ink-3">
          {mode === "token"
            ? "Needs at least org:read. The token stays in your browser."
            : mode === "signup" || mode === "sent"
              ? "You can sign in and look around straight away. Creating repositories needs the address confirmed."
              : mode === "forgot"
                ? "We answer the same way whether or not the address has an account here."
                : "Your session is an HttpOnly cookie — no script on this page can read it."}
        </p>
      </form>
    </div>
  );
}
