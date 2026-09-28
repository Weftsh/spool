import { useCallback, useEffect, useState } from "react";
import { toast } from "sonner";
import { api, loadSession, saveSession, type Me, type Session } from "./api";
import { NEW_ORG } from "@/components/app-sidebar";
import { AdminShell } from "@/shells/admin-shell";
import { ClaimInstall } from "@/components/claim-install";
import { ConnectBanner, connectOutcomeOf } from "@/components/connect-banner";
import {
  signinOutcomeOf,
  withoutSigninOutcome,
} from "@/components/signin-banner";
import { Loading } from "@/components/feedback";
import { NotFound } from "@/components/not-found";
import { type AuthMethods, loadAuthMethods } from "@/lib/auth-methods";
import { signinReturnOf, tabStorage, takeReturn } from "@/lib/return-to";
import { settingsSections } from "@/lib/settings-sections";
import { AcceptInvite, Login, ResetPassword } from "@/views/auth";
import { ChangesetsView } from "@/views/changesets";
import { NewRepo } from "@/views/newrepo";
import { NewOrg, OrgView } from "@/views/org";
import { Search } from "@/views/search";
import { SettingsView } from "@/views/settings";
import {
  DASH,
  dash,
  href,
  mountedAt,
  navigateTo,
  segments,
  useRoute,
} from "./router";
import { match } from "./routes";
import { ForgeView } from "@/views/forge";

/// A token an emailed link carries, if this is one of those links.
///
/// Every mailed credential — an invitation, a password reset, the proof
/// of an added address — rides in the fragment rather than the query, so
/// that it is never sent to a server, never lands in an access log, and
/// never travels in a `Referer` when this page loads a font. Each is read
/// once, at boot, and cleared from the address bar as soon as it has
/// been used: a bearer credential left in a URL gets bookmarked and
/// shared.
function tokenFromHash(key: string): string | null {
  const m = new RegExp(`(?:^#|&)${key}=([^&]+)`).exec(window.location.hash);
  if (!m) return null;
  try {
    return decodeURIComponent(m[1]);
  } catch {
    // A malformed escape is not a token; treat it as no link at all
    // rather than sending garbage to the server.
    return null;
  }
}

function clearHash() {
  window.history.replaceState(
    null,
    "",
    window.location.pathname + window.location.search,
  );
}

/// The sidebar writes its state to a cookie on every toggle, but the
/// stock provider only ever reads it server-side (it comes from a
/// framework with SSR). This is the read side: without it the rail
/// would reset to open on every load and "persisted per browser" would
/// be a cookie nobody consults.
function sidebarStateFromCookie(): boolean {
  const m = /(?:^|; )sidebar_state=(true|false)/.exec(document.cookie);
  return m ? m[1] === "true" : true;
}

export default function App() {
  const [session, setSession] = useState<Session | null>(null);
  const [me, setMe] = useState<Me | null>(null);
  const [booting, setBooting] = useState(true);
  const [invite, setInvite] = useState<string | null>(() =>
    tokenFromHash("invite"),
  );
  const [resetting, setResetting] = useState<string | null>(() =>
    tokenFromHash("reset"),
  );
  // The link mailed when somebody adds a second address to an account
  // they are already signed in on.
  //
  // The only address-proving link there is: the address an account signs
  // in with was proved when the account was made, by the invitation
  // reaching it or by the operator who made it. This one names an
  // account that already exists, and the server refuses it unless the
  // caller is that account — so it is redeemed here, after the session
  // is known, rather than on a signed-out screen like the others.
  //
  // Without this the mail was a promise we did not keep: the server has
  // always sent `#verify-email=`, and nothing read it, so clicking the
  // link landed on the dashboard and did nothing at all.
  const [provingEmail, setProvingEmail] = useState<string | null>(() =>
    tokenFromHash("verify-email"),
  );
  const [creatingOrg, setCreatingOrg] = useState(false);
  // How this server lets a person sign in. Null until the server has
  // answered — and it always answers, because a failure reads as the
  // screen every server offered before it could say (password and
  // GitHub). Asked at boot, beside "who am I", so the sign-in screen is
  // never drawn with a password form that disappears a moment later.
  const [methods, setMethods] = useState<AuthMethods | null>(null);
  // How a trip out to sign in — through GitHub, or through the
  // company's identity provider — came back. A query rather than a
  // fragment because our callback builds the URL, and it carries an
  // outcome, not a credential: the session rode home in an HttpOnly
  // cookie.
  //
  // Read once, here, and then taken out of the address bar (the effect
  // below). It used to be read off the live URL on every render and
  // never removed, so `?github=denied` said "you cancelled at GitHub"
  // again on every reload, and again after the person signed in with a
  // password and later signed out — about nothing they had just done.
  // `ok` and anything unknown are no outcome: a success lands signed in
  // like any other sign-in. Dropped on signing out, too: the sign-in
  // screen that follows is not the one the round trip landed on.
  const [signinOutcome, setSigninOutcome] = useState(() =>
    signinOutcomeOf(new URLSearchParams(window.location.search)),
  );
  const [browserRoute, browserNavigate] = useRoute();
  // Everything below this line is written as if the SPA were mounted at
  // `/dashboard`, because it is — the forge mounts the same views at the
  // root with a different base. Views therefore never spell the
  // prefix themselves. It is a smaller claim than it used to be: the
  // repository surfaces all live on the forge mount now, so what is
  // written relative to `/dashboard` is what is genuinely about a
  // namespace rather than about one of its repositories.
  const [route, navigate] = mountedAt(DASH, browserRoute, browserNavigate);

  // A repository has no address under here any more. It has exactly one
  // address, `/{owner}/{repo}`, and this mount links to it like anybody
  // else does — see the repository table in `OrgView`.
  //
  // There used to be two: `/dashboard/repos/{name}` rendered the file
  // browser, and the repo *screen* had no address at all because it was
  // held in `useState`, so there was nothing in the address bar to copy
  // and a reload lost the repo.
  const routed = segments(route.path);
  const routedNew = routed[0] === "new";
  // Search is its own address so a result set can be sent to somebody,
  // and so the back button returns to it rather than to the org page.
  const routedSearch = routed[0] === "search";
  const searchQuery = route.query.get("q") ?? "";
  // Settings is a family of addresses, one per section, so the sidebar
  // can link them and a section can be sent to somebody.
  const routedSettings = routed[0] === "settings";
  const settingsSection = routedSettings ? routed[1] : undefined;
  // Changesets cross repositories, so they live beside the repository
  // list rather than under any one repo. `/changesets` is the list,
  // `/changesets/<key>` one changeset — an address a review request can
  // carry.
  const routedChangesets = routed[0] === "changesets";
  const changesetKey = routedChangesets ? routed[1] : undefined;
  // How the GitHub install round trip came back. It is a query, not a
  // fragment, because GitHub builds this URL and only the redirect
  // target is ours — and it carries no credential, only an outcome.
  const connectOutcome = route.query.get("connect");
  const connectProblem = connectOutcomeOf(connectOutcome);
  // An install that began on GitHub: the installation is parked and
  // the person picks the org here, after signing in if they must.
  const claimingInstall = connectOutcome === "claim";

  // Said once, then gone: the outcome is in state (above), so the
  // address bar no longer needs it. Everything else the address carries
  // — `connect`, `org`, `next`, a mailed link's fragment — is left
  // exactly as it was. `replace`, so Back does not return to it.
  //
  // A round trip that came home signed in goes back to the page it left
  // from, when the sign-in screen it left had one (`lib/return-to.ts`):
  // `/login?next=/acme/widget` → the provider → `/acme/widget`, not the
  // overview. Any return spends the kept address, a refusal included,
  // so a later sign-in cannot find a stale one. A second run of this
  // effect (StrictMode's) finds the address already clean and does
  // nothing.
  useEffect(() => {
    const { pathname, search, hash } = window.location;
    const came = signinReturnOf(new URLSearchParams(search));
    const back = came ? takeReturn(tabStorage()) : null;
    if (came === "ok" && back) {
      navigateTo(back, true);
      return;
    }
    const rest = withoutSigninOutcome(search);
    if (rest !== search) navigateTo(`${pathname}${rest}${hash}`, true);
  }, []);

  // A mailed link opened while this page is already showing changes only
  // the fragment, which is a *same-document* navigation: the browser
  // fires `hashchange` and never reloads, so the initializers above
  // never run again. Somebody signed in with the dashboard open in a
  // tab — the ordinary case for a link proving an added address — would
  // click it and watch nothing happen at all.
  //
  // `clearHash` uses `replaceState`, which deliberately does not fire
  // this, so redeeming a link cannot re-trigger itself.
  useEffect(() => {
    const reread = () => {
      setInvite(tokenFromHash("invite"));
      setProvingEmail(tokenFromHash("verify-email"));
      setResetting(tokenFromHash("reset"));
    };
    window.addEventListener("hashchange", reread);
    return () => window.removeEventListener("hashchange", reread);
  }, []);

  // "Am I signed in?" is a request, not a lookup: the session cookie is
  // HttpOnly, so nothing here can read it. A stored API token, by
  // contrast, is right there in localStorage and needs no round trip.
  useEffect(() => {
    let alive = true;
    const asked = loadAuthMethods(api.authMethods).then(
      (m) => alive && setMethods(m),
    );
    const stored = loadSession();
    if (stored?.token) {
      // A token session has no person behind it and draws no sign-in
      // screen, so it does not wait for the answer.
      setSession(stored);
      setBooting(false);
      return () => {
        alive = false;
      };
    }
    const who = api
      .me()
      .then((who) => {
        if (!alive) return;
        setMe(who);
        const org =
          who.orgs.find((o) => o.name === stored?.org)?.name ??
          who.orgs[0]?.name ??
          "";
        setSession({ org, token: "" });
      })
      .catch(() => {
        /* not signed in — the login form is the answer */
      });
    void Promise.allSettled([asked, who]).then(
      () => alive && setBooting(false),
    );
    return () => {
      alive = false;
    };
  }, []);

  const signOut = useCallback(async () => {
    if (session && !session.token) {
      // Ending the server's session too, not just forgetting it here:
      // a cookie this browser stopped showing is still a live session
      // for anyone who has it.
      await api.logout().catch(() => undefined);
    }
    saveSession(null);
    setSession(null);
    setMe(null);
    setSigninOutcome(null);
  }, [session]);

  if (booting) return <Loading />;
  // Every screen below that offers a way in is drawn from the server's
  // answer. It is only still missing after a token session signed out
  // before it arrived — a moment, not a state.
  const signinScreen = (screen: (m: AuthMethods) => React.ReactNode) =>
    methods ? screen(methods) : <Loading />;

  const signedIn = (who: Me) => {
    const org = who.orgs[0]?.name ?? "";
    saveSession({ org, token: "" });
    setSession({ org, token: "" });
    setMe(who);
  };

  if (resetting) {
    return signinScreen((methods) => (
      <ResetPassword
        token={resetting}
        methods={methods}
        onDone={(who) => {
          clearHash();
          setResetting(null);
          signedIn(who);
        }}
        onDismissed={() => {
          clearHash();
          setResetting(null);
        }}
      />
    ));
  }

  if (invite) {
    return signinScreen((methods) => (
      <AcceptInvite
        token={invite}
        me={me}
        methods={methods}
        onAccepted={(who) => {
          clearHash();
          setInvite(null);
          signedIn(who);
        }}
        onDismissed={() => {
          clearHash();
          setInvite(null);
        }}
      />
    ));
  }

  // Which page the address means, decided *after* the mailed-link gates
  // on purpose: a reset or invitation link has to be redeemable from
  // whatever page it was opened on.
  const forge = match(browserRoute.path, browserRoute.query);
  // `/login` is the one page somebody who is not signed in may see.
  // Signing in returns to where they were, which is the whole reason
  // `next` is carried at all.
  if (forge.kind === "login") {
    const back = forge.next;
    return signinScreen((methods) => (
      <Login
        methods={methods}
        outcome={signinOutcome}
        next={back}
        onSignedIn={(s, who) => {
          if (s.token) saveSession(s);
          else saveSession({ org: s.org, token: "" });
          setSession(s);
          setMe(who);
          browserNavigate(back, true);
        }}
      />
    ));
  }
  // Redeem a mailed "prove this address" link once, as soon as we know
  // who is signed in.
  //
  // An effect rather than a render branch, because unlike the other
  // mailed links this one has no page of its own: the person is already
  // signed in and already somewhere. It proves the address, says so, and
  // leaves them where they were. `me.handle` is what the route is
  // addressed by, so it waits for the boot probe rather than firing on a
  // null handle and getting a 404 it would have to explain.
  if (provingEmail && me?.handle) {
    const handle = me.handle;
    const token = provingEmail;
    setProvingEmail(null);
    clearHash();
    api
      .proveEmail(session ?? { org: handle, token: "" }, handle, token)
      .then(({ address }) =>
        toast.success(
          `${address} is proved — commits you have already pushed under it are now attributed to you`,
        ),
      )
      // The server's own sentence. A link can be spent, expired, or for
      // somebody else's account, and those are three different things
      // for the person holding it.
      .catch((e) => toast.error(String((e as Error)?.message ?? e)));
  }

  // `/` has no page of its own. It is the way in, and the way in is the
  // dashboard — moved to rather than rendered, so the overview keeps one
  // address. `replace`, so Back does not bounce off it. A visitor who is
  // not signed in meets the dashboard's own sign-in form there.
  if (forge.kind === "home") {
    browserNavigate(dash([]), true);
    return null;
  }

  // Every other page is for somebody signed in — there is no public
  // repository, and so nothing a visitor could be shown. They are sent
  // to `/login` with the address they asked for, and signing in brings
  // them back to it. `session` rather than `me`, because a session held
  // by an API token is signed in with no person behind it.
  //
  // The dashboard mount is left to its own inline form below, which
  // also carries the "your GitHub installation is ready" notice an
  // install round trip lands with.
  if (!session && forge.kind !== "dash") {
    const q = browserRoute.query.toString();
    browserNavigate(
      href(["login"], { next: `${browserRoute.path}${q ? `?${q}` : ""}` }),
      true,
    );
    return null;
  }

  // An address that is an alias for a tab's real one — `/o/r/actions`
  // for `/o/r/checks` — moves the browser rather than rendering under
  // the name it was asked for. Two live URLs for one page is two things
  // to keep the tab strip's `active` logic agreeing about, and two to
  // share.
  //
  // `replace: true`, so Back goes where the reader came from instead of
  // bouncing off the alias and landing straight back here. `render`
  // returns null for the one frame between: rendering the page first
  // would flash the correct content at the wrong address, which is the
  // thing a shared screenshot then records.
  if (forge.kind === "repo" && forge.redirect) {
    const to = forge.redirect;
    // In an effect-free branch, so it runs during render — the same
    // shape the settings shell already uses for its own redirect to the
    // first visible section.
    browserNavigate(to, true);
    return null;
  }

  if (forge.kind !== "dash") {
    return (
      <ForgeView
        match={forge}
        me={me}
        token={session?.token ?? null}
        navigate={browserNavigate}
        onSignOut={signOut}
      />
    );
  }

  if (!session) {
    return signinScreen((methods) => (
      <Login
        methods={methods}
        outcome={signinOutcome}
        notice={
          claimingInstall
            ? "Your GitHub installation is ready to connect. Sign in, and then choose the organization it belongs to."
            : undefined
        }
        onSignedIn={(s, who) => {
          if (s.token) saveSession(s);
          else saveSession({ org: s.org, token: "" });
          setSession(s);
          setMe(who);
        }}
      />
    ));
  }
  // An unanswered question about passwords keeps the page: the answer is
  // in before anybody signed in with a cookie reaches this line.
  const passwords = methods?.password ?? true;
  const currentSection = routedSettings
    ? settingsSections(me, session.org, passwords).find(
        (x) => x.slug === settingsSection,
      )
    : undefined;

  return (
    <AdminShell
      me={me}
      org={session.org}
      sections={settingsSections(me, session.org, passwords)}
      currentPath={route.path}
      crumbs={[
        // The group the sidebar files the section under, so the crumb
        // and the rail say the same thing about whose setting it is.
        ...(currentSection
          ? [currentSection.group === "org" ? "Organization" : "Your account"]
          : routedSettings
            ? ["Settings"]
            : []),
        ...(routedChangesets ? ["Changesets"] : []),
        ...(changesetKey ? [changesetKey] : []),
        ...(currentSection ? [currentSection.label] : []),
      ]}
      defaultSidebarOpen={sidebarStateFromCookie()}
      onSwitchOrg={(v) => {
        // Creating one is an entry in the list somebody is already
        // looking at when they think "I need another namespace" — a
        // button elsewhere would be a button nobody finds. The
        // sentinel is not a name anybody can have: the reserved-name
        // denylist would refuse it, and it contains characters the
        // shape check rejects anyway.
        if (v === NEW_ORG) {
          setCreatingOrg(true);
          return;
        }
        setSession({ org: v, token: "" });
        saveSession({ org: v, token: "" });
        navigate("/");
      }}
      onNavigate={navigate}
      onSignOut={signOut}
    >
      {connectProblem && <ConnectBanner outcome={connectProblem} />}
      {claimingInstall && (
        <ClaimInstall
          me={me}
          onCreateOrg={() => setCreatingOrg(true)}
          onConnected={(org) => {
            setSession({ org, token: "" });
            saveSession({ org, token: "" });
            browserNavigate(`/?connect=ok&org=${encodeURIComponent(org)}`, true);
          }}
        />
      )}
      {creatingOrg && (
        <NewOrg
          onCancel={() => setCreatingOrg(false)}
          onCreated={async (org) => {
            setCreatingOrg(false);
            // Re-read rather than patching the list by hand: the server
            // decides what somebody belongs to, and a local guess that
            // disagreed would show an org they cannot open.
            const who = await api.me().catch(() => null);
            if (who) setMe(who);
            setSession({ org: org.name, token: "" });
            saveSession({ org: org.name, token: "" });
            // Ready at once: the new organization's own page, with the
            // server's sentence about what it holds.
            toast.message(org.detail);
            navigate("/");
          }}
        />
      )}
      {routedSettings ? (
        <SettingsView
          session={session}
          me={me}
          passwords={passwords}
          section={settingsSection}
          navigate={navigate}
        />
      ) : routedChangesets ? (
        <ChangesetsView
          session={session}
          selectedKey={changesetKey}
          navigate={navigate}
        />
      ) : routedSearch ? (
        <Search
          session={session}
          query={searchQuery}
          navigate={navigate}
          onOpenOrg={(org) => {
            // A hit in another namespace: switch to it, because every
            // repo screen is org-scoped and opening one without
            // switching would 404 in a way that reads as the search
            // having lied.
            setSession({ org, token: "" });
            saveSession({ org, token: "" });
          }}
        />
      ) : routedNew ? (
        <NewRepo
          session={session}
          navigate={navigate}
          justConnected={connectOutcome === "ok"}
        />
      ) : routed.length === 0 ? (
        <OrgView
          session={session}
          onAuthFailure={signOut}
          navigate={navigate}
          // Built here rather than inside the table, because this is the
          // one place that knows both the namespace being listed and the
          // *absolute* navigator. `navigate` above is mount-relative and
          // would have produced `/dashboard/acme/widget`.
          repoLink={(name) => {
            const to = href([session.org, name]);
            return { href: to, open: () => browserNavigate(to) };
          }}
        />
      ) : (
        // Anything else under `/dashboard` is a page that is not here.
        //
        // It used to fall through to `OrgView`, so `/dashboard/repos/widget`
        // — and `/dashboard/anything-at-all` — quietly rendered the org
        // overview under a URL that promised something else. That was
        // invisible while `repos` was a live route and is exactly the
        // wrong answer now that it is not: somebody following an old
        // link to a repository must be told the address is dead, not
        // shown a different page and left to work it out.
        //
        // The server answers `/dashboard/*path` with the SPA shell
        // whatever the path is (`webassets.rs`), so this is the only
        // place the question can be asked at all.
        <NotFound onNavigate={navigate} what={route.path.slice(1)} />
      )}
    </AdminShell>
  );
}
