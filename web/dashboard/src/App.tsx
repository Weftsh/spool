import { useCallback, useEffect, useState } from "react";
import { toast } from "sonner";
import { api, loadSession, saveSession, type Me, type Session } from "./api";
import { NEW_ORG } from "@/components/app-sidebar";
import { AdminShell } from "@/shells/admin-shell";
import { ConfirmBanner } from "@/components/confirm-banner";
import { ClaimInstall } from "@/components/claim-install";
import { ConnectBanner, connectOutcomeOf } from "@/components/connect-banner";
import { githubOutcomeOf } from "@/components/github-signin-banner";
import { Loading } from "@/components/feedback";
import { NotFound } from "@/components/not-found";
import { settingsSections } from "@/lib/settings-sections";
import { AcceptInvite, Login, RedeemLink } from "@/views/auth";
import { ChangesetsView } from "@/views/changesets";
import { NewRepo } from "@/views/newrepo";
import { NewOrg, OrgView } from "@/views/org";
import { Search } from "@/views/search";
import { SettingsView } from "@/views/settings";
import { DASH, href, mountedAt, segments, useRoute } from "./router";
import { match } from "./routes";
import { ForgeView } from "@/views/forge";

/// A token an emailed link carries, if this is one of those links.
///
/// Every mailed credential — an invitation, a confirmation, a password
/// reset — rides in the fragment rather than the query, so that it is
/// never sent to a server, never lands in an access log, and never
/// travels in a `Referer` when this page loads a font. Each is read once,
/// at boot, and cleared from the address bar as soon as it has been
/// used: a bearer credential left in a URL gets bookmarked and shared.
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
  const [confirming, setConfirming] = useState<string | null>(() =>
    tokenFromHash("verify"),
  );
  const [resetting, setResetting] = useState<string | null>(() =>
    tokenFromHash("reset"),
  );
  // The link mailed when somebody adds a second address to an account
  // they are already signed in on.
  //
  // Distinct from `verify`, which proves the address you *signed up*
  // with from a page nobody is signed in on. This one names an account
  // that already exists, and the server refuses it unless the caller is
  // that account — so it is redeemed here, after the session is known,
  // rather than by the signed-out redeemer above.
  //
  // Without this the mail was a promise we did not keep: the server has
  // always sent `#verify-email=`, and nothing read it, so clicking the
  // link landed on the dashboard and did nothing at all.
  const [provingEmail, setProvingEmail] = useState<string | null>(() =>
    tokenFromHash("verify-email"),
  );
  const [creatingOrg, setCreatingOrg] = useState(false);
  const [browserRoute, browserNavigate] = useRoute();
  // Everything below this line is written as if the SPA were mounted at
  // `/dashboard`, because it is — the public forge mounts the same views
  // at the root with a different base. Views therefore never spell the
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
  // held in `useState`. Both were unsendable. The dashboard one because
  // a stranger following it met the sign-in wall below, even for a
  // public repository; the state one because there was nothing in the
  // address bar to copy and a reload lost the repo.
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

  // How a trip through the GitHub sign-in came back. A query for the
  // same reason `connect` is: our callback builds this URL, and it
  // carries an outcome rather than a credential — the session rode home
  // in an HttpOnly cookie.
  const githubOutcome = route.query.get("github");
  const githubProblem = githubOutcomeOf(githubOutcome);
  // A brand-new account made through GitHub has exactly one thing left
  // to do, and this is the whole reason the sign-up asks through GitHub
  // at all: say which repositories to mirror. Landing them on an empty
  // overview and trusting them to find "New repository" is how an
  // onboarding that exists gets missed.
  const onboardingMirrors = githubOutcome === "new";

  // A mailed link opened while this page is already showing changes only
  // the fragment, which is a *same-document* navigation: the browser
  // fires `hashchange` and never reloads, so the initializers above
  // never run again. Somebody signed in with the dashboard open in a
  // tab — the ordinary case for a confirmation link — would click it and
  // watch nothing happen at all.
  //
  // `clearHash` uses `replaceState`, which deliberately does not fire
  // this, so redeeming a link cannot re-trigger itself.
  useEffect(() => {
    const reread = () => {
      setInvite(tokenFromHash("invite"));
      setConfirming(tokenFromHash("verify"));
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
    const stored = loadSession();
    if (stored?.token) {
      setSession(stored);
      setBooting(false);
      return;
    }
    let alive = true;
    api
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
      })
      .finally(() => alive && setBooting(false));
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
  }, [session]);

  if (booting) return <Loading />;

  const signedIn = (who: Me) => {
    const org = who.orgs[0]?.name ?? "";
    saveSession({ org, token: "" });
    setSession({ org, token: "" });
    setMe(who);
  };

  if (confirming) {
    return (
      <RedeemLink
        kind="verify"
        token={confirming}
        onDone={(who) => {
          clearHash();
          setConfirming(null);
          signedIn(who);
        }}
        onDismissed={() => {
          clearHash();
          setConfirming(null);
        }}
      />
    );
  }

  if (resetting) {
    return (
      <RedeemLink
        kind="reset"
        token={resetting}
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
    );
  }

  if (invite) {
    return (
      <AcceptInvite
        token={invite}
        me={me}
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
    );
  }

  // The public forge is decided before the session is, and that ordering
  // is the feature. Everything below this point assumes somebody signed
  // in and belongs to a namespace; a stranger following a link to an
  // open-source repository has neither, and asking them to sign in to
  // read public code would undo the whole point of hosting it.
  //
  // It sits *after* the mailed-link gates on purpose: a confirmation or
  // invitation link has to be redeemable from whatever page it was
  // opened on, including this one.
  const forge = match(browserRoute.path, browserRoute.query);
  // `/login` is the forge's own way in, and it was the one control
  // offered to a signed-out visitor that led nowhere: the header's Sign
  // in link pointed here, `match` returned a `login` route, and
  // `ForgeView` had no branch for it — so the button on every public
  // page rendered "We couldn't find that page". `safeNext` had been
  // written, and its open-redirect hole found and fixed, for a
  // destination that never rendered.
  //
  // Signing in returns to where they were, which is the whole reason
  // `next` is carried at all.
  if (forge.kind === "login") {
    const back = forge.next;
    return (
      <Login
        initialMode={forge.mode === "signup" ? "signup" : "person"}
        githubOutcome={githubProblem}
        onSignedIn={(s, who) => {
          if (s.token) saveSession(s);
          else saveSession({ org: s.org, token: "" });
          setSession(s);
          setMe(who);
          browserNavigate(back, true);
        }}
      />
    );
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
          `${address} is proved — commits you have already pushed under it now count`,
        ),
      )
      // The server's own sentence. A link can be spent, expired, or for
      // somebody else's account, and those are three different things
      // for the person holding it.
      .catch((e) => toast.error(String((e as Error)?.message ?? e)));
  }

  // An address that is an alias for a tab's real one — `/o/r/actions`
  // for `/o/r/checks` — moves the browser rather than rendering under
  // the name it was asked for. Two live URLs for one page is two things
  // to keep the tab strip's `active` logic agreeing about, two to share,
  // and two for a crawler to index as duplicates.
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
        currentPath={browserRoute.path}
        navigate={browserNavigate}
        onSignOut={signOut}
      />
    );
  }

  if (!session) {
    return (
      <Login
        githubOutcome={githubProblem}
        notice={
          claimingInstall
            ? "Your GitHub installation is ready to connect. Sign in, or create an account, and then choose the organization it belongs to."
            : undefined
        }
        onSignedIn={(s, who) => {
          if (s.token) saveSession(s);
          else saveSession({ org: s.org, token: "" });
          setSession(s);
          setMe(who);
        }}
      />
    );
  }
  const currentSection = routedSettings
    ? settingsSections(me, session.org).find((x) => x.slug === settingsSection)
    : undefined;

  return (
    <AdminShell
      me={me}
      org={session.org}
      sections={settingsSections(me, session.org)}
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
      {me && me.verified_at == null && <ConfirmBanner email={me.email} />}
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
      ) : routedNew || onboardingMirrors ? (
        <NewRepo
          session={session}
          navigate={navigate}
          justConnected={connectOutcome === "ok" || onboardingMirrors}
          // The personal namespace is the membership named by the
          // person's own handle — the server says which, rather than
          // this guessing from the list. A token session has no `me`
          // and is treated as an organization, which is what a token
          // is minted for.
          personal={me?.handle != null && me.handle === session.org}
          onCreateOrg={() => setCreatingOrg(true)}
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
