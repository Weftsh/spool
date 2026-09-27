// Manual end-to-end walkthrough: drive the REAL running stack in a real
// browser, as a person. Sign in with a password, use the Settings area,
// mint a credential and prove it authenticates, add an SSH key, revoke
// both, and check what each role is offered. Records every console
// error, page error, failed request and layout defect — the things a
// curl smoke test cannot see.
//
// This is not part of CI: it needs a running server, Postgres, MinIO and
// seeded people. Run it against a local stack after a change that
// touches the dashboard:
//
//   cd web/dashboard && npm run build
//   BASE=http://127.0.0.1:8080 \
//   CHROMIUM_PATH=/path/to/chrome node tools/walkthrough.mjs
//
// It expects three accounts in the `acme` org, all with the password
// below: ada@acme.dev (owner), dev@acme.dev (member), view@acme.dev
// (viewer) — see `stratum-server admin user-create`. It leaves the org
// as it found it: everything it creates, it revokes.
//
// The stack must also have mail captured to a directory —
// STRATUM_MAIL_TRANSPORT=capture and STRATUM_MAIL_DIR — and that same
// directory passed here as STRATUM_MAIL_DIR, so this run can open the
// invitation the way the person it was sent to does. Registering an
// invitation and seeing a link in the panel proves the form; opening the
// message and joining with what is in it proves the product.
//
// The stack must have its SSH front door configured — STRATUM_SSH_BIND,
// STRATUM_SSH_HOST_KEY (the PEM itself, not a path) and
// STRATUM_SSH_PUBLIC_URL. Without them the dashboard has no SSH clone
// URL to show and correctly hides the row, and this run reports that
// rather than passing quietly: a walkthrough against a half-configured
// deployment is a walkthrough of a different product.
import { chromium, expect } from "@playwright/test";
import { execFileSync, spawn } from "node:child_process";
import crypto from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const BASE = process.env.BASE ?? "http://127.0.0.1:8080";
const OUT =
  process.env.OUT ??
  new URL("../../../.walkthrough2", import.meta.url).pathname;
// A fresh keypair per run. Registering a fingerprint that is already
// active is a conflict — correct, and not what this walkthrough is for.
const KEYDIR = fs.mkdtempSync(path.join(os.tmpdir(), "walk-key-"));
execFileSync("ssh-keygen", [
  "-t",
  "ed25519",
  "-N",
  "",
  "-C",
  "walkthrough",
  "-f",
  path.join(KEYDIR, "id"),
]);
const PUBKEY = fs.readFileSync(path.join(KEYDIR, "id.pub"), "utf8").trim();
const MAIL_DIR = process.env.STRATUM_MAIL_DIR;
const problems = [];
const shots = [];

// Steps that provoke a refusal on purpose. A 401 for a password we typed
// wrong is the product working; recording it as a problem would bury the
// real findings under noise.
const EXPECTED = {
  // The site stage asks a published site for two things it deliberately
  // does not have, and a 404 is the whole answer being checked: a page
  // the repository has no file for, which must come back as the
  // repository's own `404.html` *with* a 404 status rather than a 200
  // search engines would index, and a file outside the published
  // directory, which must not be served at all. Both are the product
  // working. The visitor context also runs a boot probe on the sites
  // domain, where there is no session and never will be.
  "sites / a directory published and served": [401, 404],
  "login form": [401], // the boot probe: nobody is signed in yet
  // Signing up through GitHub starts and ends signed out: the boot probe
  // before the round trip, and again after the sign-out that returns the
  // pass to the state it found.
  "signing up with GitHub": [401],
  // Every GitHub refusal lands on the sign-in screen by design, so this
  // stage is signed out for all of them and meets the probe each time.
  "signing in with GitHub, refused": [401],
  // A stranger arriving at the dashboard for the first time: the same
  // boot probe, before they have an account at all.
  "signing up as a stranger": [401],
  // The recipient's browser has never been here: the same boot probe,
  // from a context with no cookie, before the invitation is accepted.
  "joining from the emailed link": [401],
  "wrong password": [401],
  // Both public-forge stages open a context that has never been here:
  // the boot probe asks "am I signed in?" and is told no, which is the
  // page working. The private-repo half of the first stage adds the
  // masking refusal for a repository a stranger may not know exists —
  // 401 rather than 404 precisely because a 404 would confirm it does.
  "the public repository page, signed out": [401],
  // The Checks tab on a repository nobody has reported a run for. The
  // boot probe is the 401; the 404 is `.../ci/poll`, which needs
  // `repo:write` and is asked by a page a stranger is reading. That
  // refusal is the product working — the tab degrades to the empty
  // state that points at the intake — but it is worth noting that the
  // *reason* it is listed is that a stranger cannot be told whether an
  // App installation is misconfigured, which is somebody's private
  // deployment detail.
  "the checks tab, signed out": [401, 404],
  "signing in from a public page returns to it": [401],
  // A signed-out context arriving from GitHub's install page: the boot
  // probe asks "am I signed in?" and is told no, which is the point.
  "an install begun on GitHub is claimed after signing in": [401],
  // A namespace owned by an **organization**, seen by a stranger.
  //
  // The 401 is the boot probe in a context that has never been here.
  // The three 404s are the person-shaped endpoints —
  // `/v1/users/:handle`, `/pins`, `/contributions` — asked about a name
  // that belongs to an org: `profiles::by_handle` resolves only
  // `orgs.kind = 'personal'`, so an org has no profile row, no pins and
  // no contribution graph, and saying so is the correct answer rather
  // than an error.
  //
  // The page is built to expect exactly this: all three reads degrade
  // to absence, so what a visitor gets is a namespace with its public
  // repositories on it and no personal furniture — which is what an
  // organization *is*. Listed here rather than fixed because there is
  // nothing to fix: the alternative is asking the client to know what
  // kind of namespace it is looking at before it is allowed to ask.
  "the public profile page, signed out": [401, 404],
  "settings / tokens": [401], // the token we just revoked, checked again
  "repo view": [404], // a viewer asking for the access map
  // Browsing starts from the repo screen, which asks for the access map
  // again — and a viewer is refused it again, by the same masking 404.
  // The 401 is the boot probe in the fresh context this step opens to
  // follow the file link the way somebody who was sent it would.
  "browsing the code": [404, 401],
  // the point of the stage: a direct commit to protected trunk is 403
  "changes / protect trunk": [403],
  // The stranger's own context, signing in from nothing: the boot probe.
  // 403: the stranger's own viewed marks. A signed-in non-member reading
  // a public repo is authorised as the public is, with no person behind
  // the read, and change_views_api refuses to keep ticks for nobody; the
  // page treats that as garnish and renders without it. A finding, not a
  // defect of this pass — see the slice 6 report.
  "workflows / a stranger's change from a fork is held until a maintainer approves it": [401, 403],
  // The second approval of the same tip. Nothing is blocked any more,
  // and 409 is the product saying so — a person who got there first is
  // not a failure.
  "workflows / a maintainer approves the fork's workflows, and they run": [409],
  // The 402 *is* the stage: a free organization asks for a private
  // repository and is told the price. The paywall that answers it is
  // what the stage then walks through, so the refusal is the product
  // working, not a request that should have succeeded.
  "billing / a private repository is a price, not a dead end": [402],
  // The same refusal from a past-due organization, asked on purpose to
  // prove a failed payment closes the door the subscription opened.
  "billing / a failed payment, settled from the portal": [402],
};

const benign = [];

function watch(page, where) {
  page.on("console", (m) => {
    if (m.type() === "error" || m.type() === "warning") {
      // Chromium logs every 4xx as a console error too; the response
      // handler below is the one that decides whether it matters.
      const bucket = /Failed to load resource/.test(m.text())
        ? benign
        : problems;
      bucket.push({
        where: where(),
        kind: `console.${m.type()}`,
        text: m.text(),
      });
    }
  });
  page.on("pageerror", (e) =>
    problems.push({ where: where(), kind: "pageerror", text: String(e) }),
  );
  page.on("requestfailed", async (r) => {
    // `net::ERR_ABORTED` is what Chromium reports for a request that
    // stopped mattering rather than one that failed: every 204 to
    // fetch() (verified with a page fulfilling its own 204), and every
    // request still in flight when the document navigates away — which
    // this run does on purpose when it leaves for the GitHub install,
    // cancelling the dashboard's own polling and a webfont mid-download.
    //
    // This does not hide a server that is down. A refusal, a reset, a
    // truncated response and a timeout each have their own errorText and
    // stay problems, and anything the server actually answered with is
    // judged by status in the handler below.
    const status = (await r.response())?.status();
    const why = r.failure()?.errorText ?? "";
    const rec = {
      where: where(),
      kind: "requestfailed",
      text: `${r.method()} ${r.url()} — ${why}`,
    };
    (status === 204 || why === "net::ERR_ABORTED" ? benign : problems).push(
      rec,
    );
  });
  page.on("response", (r) => {
    if (r.status() < 400) return;
    const rec = {
      where: where(),
      kind: `http.${r.status()}`,
      text: `${r.request().method()} ${r.url()}`,
    };
    ((EXPECTED[where()] ?? []).includes(r.status()) ? benign : problems).push(
      rec,
    );
  });
}

let stage = "boot";
async function shot(page, name, note) {
  const file = `${OUT}/${name}.png`;
  // Let CSS transitions finish first. A button re-enabled the moment a
  // 402 came back is still fading from 60% opacity to full when the
  // frame is taken, and the paywall shot showed every button on the
  // form washed out — which reads as "all disabled" to anyone looking
  // at the picture later. Bounded: an animation that never ends must
  // not hang the pass.
  await page
    .evaluate(() =>
      Promise.race([
        Promise.all(document.getAnimations().map((a) => a.finished.catch(() => {}))),
        new Promise((r) => setTimeout(r, 1000)),
      ]),
    )
    .catch(() => {});
  await page.screenshot({ path: file, fullPage: false });
  shots.push({ name, note });
  console.log(`  shot: ${name} — ${note}`);
}

// Layout sanity a human would notice at a glance.
//
// Controls are collected through open shadow roots as well as the light
// DOM: the code surfaces (`src/code/`) render inside shadow roots, and a
// `querySelectorAll` on the document stops at that boundary, so the
// tree's search box and any button the library draws would otherwise be
// invisible to the offscreen and unlabelled-button checks. The
// jammed-tag check stays on the light DOM — `innerHTML` cannot see
// through either, and the markup inside is the library's, not ours.
async function audit(page, where) {
  const out = await page.evaluate(() => {
    const r = { overflow: null, offscreen: [], emptyButtons: [], jammed: [] };
    const de = document.documentElement;
    if (de.scrollWidth > de.clientWidth + 1)
      r.overflow = `${de.scrollWidth} > ${de.clientWidth}`;
    const controls = [];
    const collect = (root) => {
      for (const el of root.querySelectorAll("*")) {
        if (el.matches("button, a, input, select")) controls.push(el);
        if (el.shadowRoot) collect(el.shadowRoot);
      }
    };
    collect(document);
    for (const el of controls) {
      const b = el.getBoundingClientRect();
      if (b.width === 0 && b.height === 0) continue;
      if (b.right > de.clientWidth + 1 || b.left < -1)
        r.offscreen.push(
          `${el.tagName.toLowerCase()} "${(el.textContent || "").trim().slice(0, 30)}"`,
        );
      if (
        el.tagName === "BUTTON" &&
        !(el.textContent || "").trim() &&
        !el.getAttribute("aria-label")
      )
        r.emptyButtons.push(el.outerHTML.slice(0, 80));
    }
    // A word run into an inline tag, e.g. "thepublic harness".
    const html = document.body.innerHTML;
    const m = html.match(/[A-Za-z0-9]{2,}<(a|span|code|strong|em)\b/g);
    if (m) r.jammed = [...new Set(m)].slice(0, 5);
    return r;
  });
  const bad = [];
  if (out.overflow) bad.push(`horizontal overflow ${out.overflow}`);
  for (const o of out.offscreen) bad.push(`offscreen control: ${o}`);
  for (const b of out.emptyButtons) bad.push(`unlabelled button: ${b}`);
  for (const j of out.jammed) bad.push(`jammed inline tag: ${j}`);
  for (const b of bad) problems.push({ where, kind: "layout", text: b });
  return bad;
}

// Headed when asked. This pass exists to be a person's-eye view and it
// reports *layout* defects, so being able to watch it is not a
// convenience — it is the difference between reading a list of problems
// and seeing the one nobody wrote a check for. `HEADED=1`, and
// `SLOWMO=<ms>` to make it followable at human speed.
const browser = await chromium.launch({
  executablePath: process.env.CHROMIUM_PATH,
  headless: !process.env.HEADED,
  slowMo: Number(process.env.SLOWMO ?? 0) || undefined,
});
const ctx = await browser.newContext({
  viewport: { width: 1440, height: 900 },
});
const page = await ctx.newPage();
watch(page, () => stage);
fs.mkdirSync(OUT, { recursive: true });

// Stages that threw. Kept separately from `problems` as well as pushed
// into it, so the summary can say "the pass did not complete" rather
// than quietly reporting on a fraction of the product.
const failedStages = [];

async function step(name, fn) {
  stage = name;
  console.log(`\n== ${name}`);
  // A throwing stage used to end the run.
  //
  // Not visibly: the process died on an unhandled rejection, no summary
  // was written, and — worse — whoever re-ran it saw the stages that had
  // passed and assumed the rest had too. That is exactly what happened
  // when the change view stopped rendering "landing is blocked while a
  // check is failing" in 0ceb3b4: the wait at `changes / ci reports
  // checks` threw, and the fifteen stages below it stopped running
  // altogether for as long as nobody read the tail of the log.
  //
  // A stage that throws is now one problem, named, with a screenshot,
  // and the pass carries on. Later stages will often be noisy because
  // the page is wherever the failure left it — that noise is the honest
  // shape of the situation, and the first entry in `failedStages` is
  // where to look. It is never a reason to stop reporting.
  try {
    await fn();
  } catch (e) {
    failedStages.push(name);
    problems.push({
      where: name,
      kind: "stage-failed",
      text: String(e).split("\n").slice(0, 4).join(" / "),
    });
    console.log(`  STAGE FAILED: ${String(e).split("\n")[0]}`);
    await shot(page, `fail-${failedStages.length}`, `${name} threw`).catch(
      () => {},
    );
  }
  // The page may be mid-navigation, or closed, after a failure; an audit
  // that throws must not do the very thing this try/catch exists to stop.
  const bad = await audit(page, name).catch(() => []);
  if (bad.length) console.log("  layout:", bad.join("; "));
}

await step("marketing home", async () => {
  await page.goto(`${BASE}/`, { waitUntil: "networkidle" });
  await shot(page, "01-home", "marketing landing page");
});

await step("docs authentication", async () => {
  await page.goto(`${BASE}/docs/authentication/`, { waitUntil: "networkidle" });
  const roles = await page.getByText("per-repo grant").first().isVisible();
  if (!roles)
    problems.push({
      where: stage,
      kind: "content",
      text: "roles section missing",
    });
  await shot(
    page,
    "02-docs-auth",
    "authentication doc, rewritten around people",
  );
});

await step("docs ssh", async () => {
  await page.goto(`${BASE}/docs/ssh/`, { waitUntil: "networkidle" });
  await shot(page, "03-docs-ssh", "ssh doc, personal key first");
});

await step('docs code review', async () => {
  await page.goto(`${BASE}/docs/code-review/`, { waitUntil: 'networkidle' });
  const grammar = await page.getByText('set noparent').first().isVisible();
  if (!grammar)
    problems.push({ where: stage, kind: 'content', text: 'the OWNERS grammar is missing from the review doc' });
  await shot(page, '03b-docs-review', 'the review doc: OWNERS grammar and the verdict table');
});

// The funnel a stranger runs, before any of the signed-in work below.
// This is the one path where nobody has vouched for you and nothing has
// been set up on your behalf, so it is the one most worth driving by
// hand: sign up, read the message, click the link, hit the wall, get
// past it. It leaves an account behind, which is what a signup does.
const NEWCOMER = `newcomer-${Date.now()}`;
const NEWCOMER_EMAIL = `${NEWCOMER}@example.dev`;

await step("signing up as a stranger", async () => {
  await page.goto(`${BASE}/dashboard/`, { waitUntil: "networkidle" });
  await page.getByRole("button", { name: "Create an account" }).click();
  await page.getByLabel("Your name").fill("New Comer");
  await page.getByLabel("Namespace").fill(NEWCOMER);
  // The namespace preview is what somebody checks before committing to
  // a name that appears in every clone URL they hand out.
  if (!(await page.getByText(`/${NEWCOMER}/repo`).isVisible())) {
    problems.push({
      where: stage,
      kind: "content",
      text: "the signup form does not show where repositories will live",
    });
  }
  await page.getByLabel("Email").fill(NEWCOMER_EMAIL);
  await page.getByLabel("Password").fill("a long enough password");
  await shot(page, "04a-signup", "signing up: name, namespace, address");
  await page.getByRole("button", { name: "Create account" }).click();
  await page.getByRole("status").waitFor({ timeout: 15000 });
  const said = await page.getByRole("status").textContent();
  console.log(`  said: ${said?.trim()}`);
  // It must not claim the account exists: the server refuses to say, and
  // a page that says otherwise contradicts it.
  if (/account is ready|welcome/i.test(said ?? "")) {
    problems.push({
      where: stage,
      kind: "content",
      text: `signup claimed more than the server will confirm: ${said}`,
    });
  }
  // Nothing left to fill in: the form is gone, and so is every other
  // form — the screen once fell through to "Organization / API token".
  for (const label of ["Password", "API token", "Organization"]) {
    if ((await page.getByLabel(label).count()) > 0)
      problems.push({
        where: stage,
        kind: "content",
        text: `after signing up the screen still shows a "${label}" field`,
      });
  }
  if ((await page.getByRole("button", { name: "Create account" }).count()) > 0)
    problems.push({
      where: stage,
      kind: "content",
      text: "after signing up the screen still offers Create account",
    });
  await shot(page, "04b-signup-sent", "the same answer either way, and nothing left to fill in");
});

await step("confirming from the message", async () => {
  if (!MAIL_DIR) return;
  let mail = null;
  for (let i = 0; i < 50 && !mail; i++) {
    for (const name of fs.readdirSync(MAIL_DIR)) {
      if (!name.endsWith(".json")) continue;
      const m = JSON.parse(fs.readFileSync(path.join(MAIL_DIR, name), "utf8"));
      if (m.to === NEWCOMER_EMAIL) mail = m;
    }
    if (!mail) await new Promise((r) => setTimeout(r, 100));
  }
  if (!mail) {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `no confirmation message for ${NEWCOMER_EMAIL}`,
    });
    return;
  }
  console.log(`  subject: ${mail.subject}`);

  // Sign in *before* confirming, and prove the wall is really there —
  // this is the state the whole verification feature exists to create,
  // and it has never been seen in a browser until now. The form is still
  // on the signup screen showing "check your email", so the way back is
  // the same link a real person would take.
  await page.getByRole("button", { name: "Sign in instead" }).click();
  await page.getByLabel("Email").fill(NEWCOMER_EMAIL);
  await page.getByLabel("Password").fill("a long enough password");
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  await page.getByText("Requests today").waitFor({ timeout: 15000 });
  const banner = page.getByRole("status").filter({ hasText: "Confirm" });
  if (!(await banner.isVisible())) {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "an unconfirmed account is not told that anything is blocked",
    });
  }
  await shot(page, "04c-unconfirmed", "signed in, and told what is blocked");

  const link = mail.text.match(/https?:\/\/\S+/)?.[0];
  if (!link || !link.startsWith(BASE)) {
    problems.push({
      where: stage,
      kind: "content",
      text: `the confirmation link points somewhere else: ${link}`,
    });
    return;
  }
  await page.goto(link, { waitUntil: "networkidle" });
  await page.getByText("Requests today").waitFor({ timeout: 15000 });
  if (new URL(page.url()).hash !== "") {
    problems.push({
      where: stage,
      kind: "security",
      text: `the confirmation token is still in the address bar: ${page.url()}`,
    });
  }
  if (
    await page
      .getByText(/to create repositories/)
      .isVisible()
      .catch(() => false)
  ) {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "the banner survived confirmation",
    });
  }
  await shot(page, "04d-confirmed", "confirmed — the banner is gone");
  await page.getByRole("button", { name: "Sign out" }).click();
  await page.getByLabel("Email").waitFor({ timeout: 10000 });
});

// Signing up through GitHub is the fast path, and the whole claim it
// makes is that there is no confirmation mail in it: GitHub has already
// proved the address, so the account is usable the moment it lands.
// Both halves of that are checked here — the banner that nags an
// unproved account must be absent, and the screen must be the one that
// asks what to mirror, because being dropped on an empty overview is
// how an onboarding that exists gets missed.
await step("signing up with GitHub", async () => {
  await page.goto(`${BASE}/dashboard/`, { waitUntil: "networkidle" });
  await shot(page, "04e-github-button", "the fast way in, above the form");
  // A real leave and a real return: the button goes to our start route,
  // which parks the anti-CSRF cookie and sends the browser to the
  // provider, and the provider redirects back to our callback. Nothing
  // here is stubbed, so the cookie check is a check this really passes.
  await page.getByRole("link", { name: "Continue with GitHub" }).click();
  await page.waitForURL(/\/dashboard\//, { timeout: 20000 });

  const url = page.url();
  if (!/github=new/.test(url)) {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `signing up with GitHub did not land on a new account: ${url}`,
    });
    return;
  }
  // The point of the feature. An account made this way is proved, so the
  // "confirm your email" banner must not be on screen.
  if (
    await page
      .getByText(/confirm your email/i)
      .first()
      .isVisible()
      .catch(() => false)
  ) {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "an account GitHub proved is still being asked to confirm its address",
    });
  }
  // And it lands on the thing the sign-up went to GitHub *for*.
  await page
    .getByRole("button", { name: /Install the GitHub app|Back/ })
    .first()
    .waitFor({ timeout: 15000 });
  await shot(
    page,
    "04f-github-onboard",
    "proved on arrival, and asked what to mirror",
  );

  // It can create immediately — the gate every other path only passes
  // after a link is clicked. Proved by doing it, not by reading a flag.
  await page.goto(`${BASE}/dashboard/new`, { waitUntil: "networkidle" });
  await page.getByRole("tab", { name: "Empty repository" }).click();
  await page.getByLabel("Repository name").fill("first-repo");
  await page.getByRole("button", { name: "Create repository" }).click();
  await page.getByText("first-repo").first().waitFor({ timeout: 15000 });
  await shot(
    page,
    "04g-github-created",
    "no confirmation mail, and creating works at once",
  );

  await page.getByRole("button", { name: "Sign out" }).click();
  await page.getByLabel("Email").waitFor({ timeout: 10000 });
});

// Every refusal lands on the signed-out screen, which is the only place
// that can say what happened — the signed-in shell's banner never sees
// one. A screen that renders the outcome silently is the failure this
// stage exists to catch.
await step("signing in with GitHub, refused", async () => {
  for (const [outcome, needle] of [
    ["noemail", /confirmed/i],
    ["emailtaken", /different account/i],
    ["denied", /cancelled at GitHub/i],
  ]) {
    await page.goto(`${BASE}/dashboard/?github=${outcome}`, {
      waitUntil: "networkidle",
    });
    const said = await page
      .getByRole("status")
      .first()
      .textContent()
      .catch(() => null);
    if (!said || !needle.test(said)) {
      problems.push({
        where: stage,
        kind: "content",
        text: `github=${outcome} says nothing a person can act on: ${said}`,
      });
    }
    // The way in must still be there. A refusal that also hides the
    // password form is a dead end.
    if ((await page.getByLabel("Email").count()) === 0) {
      problems.push({
        where: stage,
        kind: "behaviour",
        text: `github=${outcome} left no other way to sign in`,
      });
    }
  }
  await shot(page, "04h-github-refused", "a refusal that still leaves a way in");
});

await step("login form", async () => {
  await page.goto(`${BASE}/dashboard/`, { waitUntil: "networkidle" });
  await page.getByLabel("Email").waitFor();
  await shot(page, "04-login", "password sign-in is the default");
});

await step("wrong password", async () => {
  await page.getByLabel("Email").fill("ada@acme.dev");
  await page.getByLabel("Password").fill("not the password");
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  await page.getByRole("alert").waitFor();
  const msg = await page.getByRole("alert").textContent();
  console.log(`  refusal: ${msg}`);
  await shot(page, "05-login-refused", "a wrong password is refused visibly");
});

await step("sign in as owner", async () => {
  await page.getByLabel("Password").fill("a long enough password");
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  await page.getByText("Requests today").waitFor({ timeout: 15000 });
  await shot(page, "06-overview", "org overview, signed in as a person");
  const stored = await page.evaluate(() => JSON.stringify(localStorage));
  if (stored.includes("a long enough password") || stored.includes("weft_"))
    problems.push({
      where: stage,
      kind: "security",
      text: `credential in localStorage: ${stored}`,
    });
  console.log(`  localStorage: ${stored}`);
});

// The step's automatic audit is the point: the icon rail must keep
// every control on-canvas and labelled, and the collapsed layout must
// not overflow. Expanded again before moving on — later steps address
// the sidebar's search box as .first(), which assumes it is visible.
await step("sidebar collapses to an icon rail", async () => {
  await page.getByRole("button", { name: "Toggle sidebar" }).click();
  await page.waitForTimeout(400);
  if (await page.getByLabel("Search repositories").first().isVisible())
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "the collapsed rail still shows the search box",
    });
  await shot(page, "06b-sidebar-collapsed", "the icon rail");
  await page.getByRole("button", { name: "Toggle sidebar" }).click();
  await page.getByLabel("Search repositories").first().waitFor();
});

// Billing first, because it is what the rest of Settings rests on:
// an organization without a subscription holds no private repository,
// and no card is asked for before one.
//
// The stack seeds acme the way a customer arrives — subscribed through
// the fake provider's subscription page — so this stage checks the paid
// state and then walks a brand-new organization through the whole
// lifecycle with a person's eyes: create it from the switcher (free at
// once, no provider in between), hit the paywall on a private
// repository, be sent to the provider's subscription page, come back,
// finish the repository from Billing, watch the payment fail and settle
// from the portal. Every one of those is a
// screen a customer will see, and none of them can be reached by
// signing a webhook from here: the point is that the *provider's* pages
// send people back to the right place.
//
// Needs the stack's fake Stripe (scripts/manual-stack/fake-stripe.py).
// Without one the product correctly says there is nothing to buy, and
// this stage says so rather than passing quietly.
await step("settings / billing", async () => {
  const tab = page.getByRole("link", { name: "Billing" });
  if (!(await tab.isVisible().catch(() => false))) {
    problems.push({
      where: stage,
      kind: "harness",
      text: "no Billing tab — is this account an owner of the org?",
    });
    return;
  }
  await tab.click();
  const status = page.getByRole("definition").filter({
    hasText: /^(Subscribed|Free|Payment failed)$/,
  });
  await status.waitFor({ timeout: 10000 });
  const before = (await status.textContent())?.trim();
  console.log(`  acme: ${before}`);
  if (before !== "Subscribed") {
    problems.push({
      where: stage,
      kind: "harness",
      text: `acme should have been seeded subscribed through the fake provider; the billing page says "${before}"`,
    });
  }
  // Both numbers have to be on screen: the difference between them is
  // the only way anybody notices a seat update that did not land.
  // "Period" once the server sends both ends of the billing period,
  // "Renews" against one that sends only the end.
  for (const label of ["Seats in use", "Seats billed", /^(Period|Renews)$/, "Per month"]) {
    if (!(await page.getByText(label, { exact: true }).first().isVisible())) {
      problems.push({
        where: stage,
        kind: "content",
        text: `the billing screen does not show "${label}"`,
      });
    }
  }
  // And the amount: three seats at the stack's $4 is $12, and a page
  // that shows the count without the product leaves the arithmetic to
  // the person paying.
  const amount = page.getByRole("definition").filter({ hasText: /^\$12/ });
  if (!(await amount.isVisible().catch(() => false)))
    problems.push({
      where: stage,
      kind: "content",
      text: "the subscribed billing page does not show the monthly amount ($12 for 3 seats at $4)",
    });
  await shot(
    page,
    "07a-billing-subscribed",
    "acme, subscribed — seats in use against seats billed",
  );
});

// The pool a seat buys, and the limit past it. acme is seeded with
// three seats, so at the stack's numbers the pools read 3,000 minutes,
// 30 GB of transfer and 15 GB of storage — and a spend limit of $0,
// which is the product's default and the one a new customer meets.
// Raising it is a PATCH the page makes; the stage reads the number
// back through the API rather than trusting the confirmation, then
// puts it back so every stage after this sees the stack as seeded.
await step("billing / three meters and a spend limit that starts at $0", async () => {
  await page.goto(`${BASE}/dashboard/settings/billing`, { waitUntil: "networkidle" });
  for (const label of ["Hosted CI minutes", "Private transfer", "Private storage"]) {
    if (!(await page.getByRole("heading", { name: label }).isVisible().catch(() => false)))
      problems.push({ where: stage, kind: "content", text: `the billing screen has no "${label}" meter` });
  }
  for (const pool of [/of 3,000 minutes/, /of 30 GB/, /of 15 GB/]) {
    if (!(await page.getByText(pool).first().isVisible().catch(() => false)))
      problems.push({ where: stage, kind: "content", text: `the billing screen does not show the pool ${pool}` });
  }
  const limit = page.getByRole("definition").filter({ hasText: /^\$0$/ });
  if (!(await limit.first().isVisible().catch(() => false)))
    problems.push({ where: stage, kind: "content", text: "the spend limit does not read $0 on a freshly seeded organization" });
  await shot(page, "07a2-billing-meters", "three meters against the pool, and a spend limit at its $0 default");

  const input = page.getByLabel("New limit, in dollars");
  if (!(await input.isVisible().catch(() => false))) {
    problems.push({ where: stage, kind: "behaviour", text: "an owner is offered no spend-limit editor" });
    return;
  }
  await input.fill("25");
  await page.getByRole("button", { name: "Save spend limit" }).click();
  await page.getByRole("status").filter({ hasText: "Spend limit is now $25." }).waitFor({ timeout: 10000 });
  const bill = await page.evaluate(async () => (await fetch("/v1/orgs/acme/billing")).json());
  console.log(`  spend limit after save: ${bill.spend_limit_cents} cents, estimated past the pool ${bill.overage_estimated_cents}`);
  if (bill.spend_limit_cents !== 2500)
    problems.push({ where: stage, kind: "behaviour", text: `the page said $25 but the API reads spend_limit_cents=${bill.spend_limit_cents}` });
  await shot(page, "07a3-billing-spend-limit", "the limit raised to $25, confirmed from the API");
  await input.fill("0");
  await page.getByRole("button", { name: "Save spend limit" }).click();
  await page.getByRole("status").filter({ hasText: "Spend limit is now $0." }).waitFor({ timeout: 10000 });
});

// At the cap. Not a mocked answer: `scripts/manual-stack.sh overage`
// writes forty gigabytes of transfer into the same table a real clone
// writes, against one of acme's private repositories, and everything
// downstream — the meter on the billing page, the clone door, the
// banner on the repository — reads that table. With the limit at $0
// a clone of a private repository is refused with the sentence that
// names the way out; with the limit at $25 the same clone lands, and
// the ten gigabytes past the pool are what the invoice will carry.
// Public work is never touched, and the stage proves that too.
const STACK = new URL("../../../scripts/manual-stack.sh", import.meta.url).pathname;
const CAP_REPO = `capvault-${Date.now().toString(36)}`;
await step("billing / at the cap, each refusal names the way out", async () => {
  // A private repository with something in it, so a clone has bytes to
  // move — and a public one, so "public is unaffected" is a clone that
  // happens rather than a sentence on a page.
  const made = await page.evaluate(async (name) => {
    const post = async (path, body) => {
      const r = await fetch(path, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
      return r.status;
    };
    const out = {};
    out.private = await post("/v1/orgs/acme/repos", { name, public: false, description: "private, for the cap" });
    out.privateCommit = await post(`/v1/orgs/acme/repos/${name}/commits`, {
      message: "ledger", operations: [{ op: "put", path: "README.md", content: "# ledger\n" }],
    });
    out.public = await post("/v1/orgs/acme/repos", { name: `${name}-open`, public: true, description: "public, unaffected by the cap" });
    out.publicCommit = await post(`/v1/orgs/acme/repos/${name}-open/commits`, {
      message: "open", operations: [{ op: "put", path: "README.md", content: "# open\n" }],
    });
    const t = await fetch("/v1/orgs/acme/tokens", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ scopes: ["repo:read"], label: `cap-${name}` }) });
    out.token = t.status === 201 || t.status === 200 ? (await t.json()).token : null;
    return out;
  }, CAP_REPO);
  console.log(`  repos: ${JSON.stringify({ ...made, token: made.token ? "minted" : "none" })}`);
  if (made.private !== 201 || made.public !== 201 || !made.token) {
    problems.push({ where: stage, kind: "harness", text: `could not set the stage: ${JSON.stringify(made)}` });
    return;
  }
  const clone = (repo) => {
    const dest = fs.mkdtempSync(path.join(os.tmpdir(), "walk-cap-"));
    const url = new URL(`/acme/${repo}.git`, BASE);
    url.username = "walkthrough";
    url.password = made.token;
    try {
      execFileSync("git", ["clone", "--quiet", url.toString(), dest], {
        env: { ...process.env, GIT_TERMINAL_PROMPT: "0" }, stdio: "pipe", timeout: 60000,
      });
      execFileSync("git", ["-C", dest, "fsck", "--full", "--strict"], { stdio: "pipe" });
      return { ok: true };
    } catch (e) {
      return { ok: false, err: String(e.stderr ?? e).replace(/\s+/g, " ").slice(0, 400) };
    }
  };
  // Below the pool first: the private repository clones today.
  const before = clone(CAP_REPO);
  if (!before.ok) {
    problems.push({ where: stage, kind: "behaviour", text: `a private repository inside the pool did not clone: ${before.err}` });
    return;
  }
  try {
    execFileSync("bash", [STACK, "overage", "acme", "40"], { stdio: "pipe", env: process.env });
  } catch (e) {
    problems.push({
      where: stage, kind: "harness",
      text: `scripts/manual-stack.sh overage failed — the cap stage needs the stack's database: ${String(e.stderr ?? e).replace(/\s+/g, " ").slice(0, 300)}`,
    });
    return;
  }
  try {
    await page.goto(`${BASE}/dashboard/settings/billing`, { waitUntil: "networkidle" });
    const note = page.getByRole("status").filter({ hasText: /refused|spend limit/i });
    if (!(await note.first().isVisible().catch(() => false)))
      problems.push({ where: stage, kind: "content", text: "past the pool at a $0 limit, the transfer meter carries no refusal note" });
    const bill = await page.evaluate(async () => (await fetch("/v1/orgs/acme/billing")).json());
    console.log(`  at the cap: egress ${JSON.stringify(bill.meters?.egress_gb)}`);
    if (!bill.meters?.egress_gb?.refusing)
      problems.push({ where: stage, kind: "behaviour", text: `40 GB against a 30 GB pool at a $0 limit and the API does not say refusing: ${JSON.stringify(bill.meters?.egress_gb)}` });
    await shot(page, "07a4-billing-at-the-cap", "past the pool with the limit at $0: the transfer meter says what is refused and why");

    const refused = clone(CAP_REPO);
    if (refused.ok)
      problems.push({ where: stage, kind: "behaviour", text: "a private clone past the pool at a $0 spend limit was served" });
    else if (!/quota:/.test(refused.err))
      problems.push({ where: stage, kind: "behaviour", text: `the refused clone does not carry the quota sentence: ${refused.err}` });
    else console.log(`  refused, as it should be: ${refused.err.slice(0, 160)}`);
    const open = clone(`${CAP_REPO}-open`);
    if (!open.ok)
      problems.push({ where: stage, kind: "behaviour", text: `the cap reached a public repository: ${open.err}` });

    // The repository screen says so too, and names the way out.
    await page.goto(`${BASE}/acme/${CAP_REPO}`, { waitUntil: "networkidle" });
    // The banner arrives with the billing read the page makes after it
    // has drawn, so it is waited for, bounded — an instant look saw the
    // tree before the read landed and called a working banner missing.
    const wall = page.getByRole("region", { name: "Spend limit reached" });
    if (!(await wall.waitFor({ timeout: 10000 }).then(() => true).catch(() => false)))
      problems.push({ where: stage, kind: "content", text: "the private repository at the cap shows no 'Spend limit reached' banner" });
    else if (!(await wall.getByRole("link", { name: "Raise the spend limit" }).isVisible().catch(() => false)))
      problems.push({ where: stage, kind: "content", text: "the banner offers an owner no way to raise the limit" });
    await shot(page, "07a5-repo-at-the-cap", "the repository at the cap: refused, with the way out on the screen");

    // Raise the limit: ten gigabytes past a thirty-gigabyte pool at the
    // stack's $0.10 is a dollar, well inside $25.
    await page.goto(`${BASE}/dashboard/settings/billing`, { waitUntil: "networkidle" });
    await page.getByLabel("New limit, in dollars").fill("25");
    await page.getByRole("button", { name: "Save spend limit" }).click();
    await page.getByRole("status").filter({ hasText: "Spend limit is now $25." }).waitFor({ timeout: 10000 });
    const after = clone(CAP_REPO);
    if (!after.ok)
      problems.push({ where: stage, kind: "behaviour", text: `with the limit at $25 the private clone is still refused: ${after.err}` });
    else console.log("  limit raised: the private clone lands and fsck is clean");
    const billed = await page.evaluate(async () => (await fetch("/v1/orgs/acme/billing")).json());
    console.log(`  estimated past the pool: ${billed.overage_estimated_cents} cents`);
    if (!(billed.overage_estimated_cents > 0))
      problems.push({ where: stage, kind: "behaviour", text: `ten gigabytes past the pool estimate ${billed.overage_estimated_cents} cents` });
    await shot(page, "07a6-billing-limit-raised", "the limit at $25: the clone lands and the overage is estimated");
  } finally {
    // Back to the seeded shape for everything that follows.
    await page.goto(`${BASE}/dashboard/settings/billing`, { waitUntil: "networkidle" });
    await page.getByLabel("New limit, in dollars").fill("0");
    await page.getByRole("button", { name: "Save spend limit" }).click();
    await page.getByRole("status").filter({ hasText: "Spend limit is now $0." }).waitFor({ timeout: 10000 }).catch(() => {});
    try {
      execFileSync("bash", [STACK, "overage", "acme", "clear"], { stdio: "pipe", env: process.env });
    } catch (e) {
      problems.push({ where: stage, kind: "harness", text: `could not clear the seeded overage: ${String(e.stderr ?? e).slice(0, 200)}` });
    }
  }
});

// A name nobody has used: the org is created for real, and an earlier
// run against the same database owns the last one.
const WALK_ORG = `walk-${Date.now().toString(36)}`;

await step("billing / a new organization is free at once", async () => {
  await page.getByLabel("Organization").click();
  await page.getByRole("option", { name: "+ New organization…" }).click();
  // Said before the button: no card, and a free namespace already
  // exists. The card-first form used to announce a trip to the
  // provider here; a merchant-of-record account has no such page.
  for (const line of [/Your own namespace is free/, /No card is needed/]) {
    if (!(await page.getByText(line).isVisible()))
      problems.push({
        where: stage,
        kind: "content",
        text: `the new-organization form does not say ${line}`,
      });
  }
  if (await page.getByText(/A card is required/).isVisible())
    problems.push({
      where: stage,
      kind: "content",
      text: "the new-organization form still says a card is required",
    });
  await page.getByLabel("Organization name").fill(WALK_ORG);
  await shot(page, "07b-new-org", "creating an organization: free, no card");
  await page
    .getByRole("button", { name: "Create organization", exact: true })
    .click();
  // Straight to the new organization: no provider in between.
  try {
    await page.getByText(/Ready\. Public repositories and members are free/).waitFor({ timeout: 15000 });
  } catch {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `creating an organization did not say it was ready; at ${page.url()}: ${(await page.innerText("body")).replace(/\s+/g, " ").slice(0, 300)}`,
    });
    return;
  }
  if (new URL(page.url()).origin !== new URL(BASE).origin)
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `creating an organization left for ${page.url()}; there is no card page any more`,
    });
  const org = (
    await page.getByRole("combobox", { name: "Organization" }).textContent()
  )?.trim();
  if (org !== WALK_ORG)
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `after creating it the dashboard shows organization "${org}", not the one just created (${WALK_ORG})`,
    });
  await page.goto(`${BASE}/dashboard/settings/billing`, { waitUntil: "networkidle" });
  await page
    .getByRole("definition")
    .filter({ hasText: /^Free$/ })
    .waitFor({ timeout: 10000 });
  const text = (await page.innerText("body")).replace(/\s+/g, " ");
  for (const want of ["per seat per month", "Continue to checkout"]) {
    if (!text.includes(want))
      problems.push({
        where: stage,
        kind: "content",
        text: `the free organization's billing page does not show "${want}"`,
      });
  }
  for (const gone of ["Manage card", "No card on file", "Add a card"]) {
    if (text.includes(gone))
      problems.push({
        where: stage,
        kind: "content",
        text: `the free organization's billing page still shows "${gone}"`,
      });
  }
  await shot(page, "07d-billing-free", "free: public repositories and people, a price for private ones, no card");
});

await step("billing / a private repository is a price, not a dead end", async () => {
  await page.goto(`${BASE}/dashboard/new`, { waitUntil: "networkidle" });
  await page.getByRole("tab", { name: "Empty repository" }).click();
  await page.getByLabel("Repository name").fill("vault");
  // Unticked: private is what this creates, and what a free org refuses.
  await page.getByRole("button", { name: "Create repository" }).click();
  const wall = page.getByRole("region", { name: "Subscription needed" });
  try {
    await wall.waitFor({ timeout: 10000 });
  } catch {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "a private repository on a free organization did not raise the paywall",
    });
    return;
  }
  // The box opens on "Reading the price…" and fills in when the billing
  // read lands; reading it the instant it appears saw the placeholder
  // and reported a paywall with no price on it. Wait for the number a
  // person waits for — bounded, so a price that never arrives is still
  // a finding rather than a hang.
  await wall.getByText(/\$\d/).waitFor({ timeout: 10000 }).catch(() => {});
  const said = (await wall.innerText()).replace(/\s+/g, " ");
  console.log(`  paywall: ${said}`);
  if (!/\$\d/.test(said) || !/per seat/.test(said))
    problems.push({
      where: stage,
      kind: "content",
      text: `the paywall does not quote a price: ${said}`,
    });
  await shot(page, "07e-paywall", "the 402, answered with the price and one button");
  if (!/promotion code/.test(said))
    problems.push({
      where: stage,
      kind: "content",
      text: "the paywall does not say that a promotion code can be entered on the provider's page",
    });
  await wall.getByRole("button", { name: "Continue to checkout" }).click();
  // On the provider's subscription page now — the fake's, at its own
  // port — with the seats, the price, the saved card and the
  // promotion-code box the real page has when `allow_promotion_codes`
  // is on. The code is the fake's one: a real account's is created by
  // hand in Stripe and typed here by a person.
  try {
    await page.getByRole("heading", { name: "Subscribe" }).waitFor({ timeout: 15000 });
  } catch {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `continuing from the paywall did not leave for the provider's subscription page; at ${page.url()}`,
    });
    return;
  }
  if (new URL(page.url()).origin === new URL(BASE).origin)
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `the subscription page is on our origin, not the provider's: ${page.url()}`,
    });
  const offer = (await page.innerText("body")).replace(/\s+/g, " ");
  console.log(`  checkout: ${offer}`);
  if (!/1 seat at \$4/.test(offer) || !/\$4\/month/.test(offer))
    problems.push({
      where: stage,
      kind: "content",
      text: `the subscription page does not quote one seat at $4: ${offer}`,
    });
  await shot(page, "07e2-provider-subscribe", "the provider's page (fake): seats, price, saved card, promotion code");
  await page.getByLabel("Promotion code").fill("WEFT100");
  await page.getByRole("button", { name: "Subscribe" }).click();

  // Back on our billing page, told the trip's outcome — once the
  // provider's webhook has landed, which the page waits for rather than
  // telling somebody who just paid that they have not.
  try {
    await page.getByRole("status").filter({ hasText: /^Subscribed\./ }).waitFor({ timeout: 20000 });
  } catch {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `after the provider's page the billing screen never said "Subscribed."; at ${page.url()}: ${(await page.innerText("body")).replace(/\s+/g, " ").slice(0, 300)}`,
    });
    return;
  }
  const back = new URL(page.url());
  if (back.pathname !== "/dashboard/settings/billing" || back.searchParams.get("subscribed") !== "done")
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `checkout returned to ${page.url()}, not /dashboard/settings/billing?subscribed=done`,
    });
  // Not a dead end: the create that was refused is offered, by name,
  // and one click finishes it. Nobody typed "vault" twice.
  const resume = page.getByRole("region", { name: "Pick up where you left off" });
  try {
    await resume.waitFor({ timeout: 10000 });
  } catch {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "back from the provider, the billing screen does not offer to create the repository that was refused",
    });
    return;
  }
  await shot(page, "07f-billing-resume", "subscribed, and offered the private repository that was refused");
  await resume.getByRole("button", { name: "Create vault (private)" }).click();
  // The clone command is the proof, as it is on the new-repository
  // form — an empty repository's own page has no tree to show yet.
  try {
    await page.getByText(/git clone/).waitFor({ timeout: 15000 });
  } catch {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `finishing the refused create did not hand over the clone command; at ${page.url()}: ${(await page.innerText("body")).replace(/\s+/g, " ").slice(0, 300)}`,
    });
    return;
  }
  await shot(page, "07g-paywall-landed", "the private repository exists, created from the billing screen; the clone command is handed over");
  const bill = await page.evaluate(async (o) => (await fetch(`/v1/orgs/${o}/billing`)).json(), WALK_ORG);
  console.log(`  billing after subscribe: ${JSON.stringify(bill)}`);
  if (bill.plan !== "paid" || bill.paid_seats !== 1 || bill.may_create_private !== true)
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `after subscribing, billing reads plan=${bill.plan} paid_seats=${bill.paid_seats} may_create_private=${bill.may_create_private}`,
    });
  const vault = await page.evaluate(async (o) => (await fetch(`/v1/orgs/${o}/repos/vault`)).json(), WALK_ORG);
  if (vault.public !== false)
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `vault was created but is not private: ${JSON.stringify(vault)}`,
    });
});

await step("billing / a failed payment, settled from the portal", async () => {
  await page.goto(`${BASE}/dashboard/settings/billing`, { waitUntil: "networkidle" });
  await page.getByRole("definition").filter({ hasText: /^Subscribed$/ }).waitFor({ timeout: 10000 });
  await page.getByRole("button", { name: "Manage billing" }).click();
  await page.getByRole("heading", { name: "Billing portal" }).waitFor({ timeout: 15000 });
  await shot(page, "07g-provider-portal", "the provider's portal (fake)");
  await page.getByRole("button", { name: "Fail the next payment" }).click();
  // The portal's return URL is our billing page; the org is now past
  // due and the page has to say what still works and what stopped.
  await page.getByRole("definition").filter({ hasText: /^Payment failed$/ }).waitFor({ timeout: 15000 });
  const text = (await page.innerText("body")).replace(/\s+/g, " ");
  for (const want of ["still readable", "nothing has been deleted", "private repositories are paused", "Settle the payment"]) {
    if (!text.includes(want))
      problems.push({
        where: stage,
        kind: "content",
        text: `the past-due billing page does not say "${want}"`,
      });
  }
  await shot(page, "07h-billing-past-due", "payment failed: readable, nothing deleted, hosted CI paused for private repos");
  // A private repository is refused now, with the same paywall
  // pointing at the portal rather than at a subscribe button.
  const refused = await page.evaluate(async (o) => {
    const r = await fetch(`/v1/orgs/${o}/repos`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ name: "vault-2", public: false }),
    });
    return { status: r.status, body: await r.text() };
  }, WALK_ORG);
  if (refused.status !== 402)
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `a past-due organization was allowed a private repository: ${refused.status} ${refused.body}`,
    });
  await page.getByRole("button", { name: "Settle the payment" }).click();
  await page.getByRole("heading", { name: "Billing portal" }).waitFor({ timeout: 15000 });
  await page.getByRole("button", { name: "Pay the open invoice" }).click();
  await page.getByRole("definition").filter({ hasText: /^Subscribed$/ }).waitFor({ timeout: 15000 });
  await shot(page, "07i-billing-settled", "settled: subscribed again");
  console.log("  the new organization went card → free → paid → past due → paid");

  // Back to acme for everything that follows.
  await page.getByLabel("Organization").click();
  await page.getByRole("option", { name: "acme", exact: true }).click();
  await page.getByText("Requests today").waitFor({ timeout: 15000 });
});

await step("settings / members", async () => {
  await page.getByRole("link", { name: "Members" }).click();
  await page.getByText("Dev Person").waitFor();
  // What the next person costs, on the screen where somebody is about
  // to add one. A paywall that only speaks when you cross it is a
  // paywall that surprises people.
  const seats = page.getByText(/seats? in use, \d+ billed/);
  // Waited for, not sampled. `SeatLine` runs its own `api.billing()`
  // call, so waiting for "Dev Person" waits for the *roster* and says
  // nothing about whether the seat line has arrived — and `isVisible()`
  // does not wait at all. Whichever of the two requests won the race
  // decided whether this reported a problem, so the same stack
  // reported "the members screen does not say what a seat costs" on one
  // run and reported nothing on the next, with no product change
  // between them. A gate that invents a problem intermittently is one
  // people learn to ignore, and it makes a genuinely missing seat line
  // indistinguishable from a slow fetch.
  try {
    await seats.waitFor({ state: "visible", timeout: 10000 });
    console.log(`  seats: ${(await seats.textContent())?.trim()}`);
  } catch {
    problems.push({
      where: stage,
      kind: "content",
      text: "the members screen does not say what a seat costs",
    });
  }
  await shot(page, "07-members", "the roster with a role control per person");
  await page.getByLabel("Role for dev@acme.dev").click();
  await page.getByRole("option", { name: "admin" }).click();
  await page.waitForTimeout(500);
  const now = (await page.getByLabel("Role for dev@acme.dev").textContent())?.trim();
  if (now !== "admin")
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `role did not stick: ${now}`,
    });
  await page.getByLabel("Role for dev@acme.dev").click();
  await page.getByRole("option", { name: "member" }).click();
  await page.waitForTimeout(500);
});

// A fresh address each run: an outstanding invitation for the same
// person is refused, which is correct and not what these steps are for.
const NEWHIRE = `newhire-${Date.now()}@acme.dev`;
let emailedLink = null;

await step("settings / invite", async () => {
  await page.getByLabel("Invite email").fill(NEWHIRE);
  await page.getByLabel("Invite role").click();
  await page.getByRole("option", { name: "member" }).click();
  await page.getByRole("button", { name: "Invite" }).click();
  await page.getByText(/^stinv_/).waitFor({ timeout: 10000 });
  const link = await page.getByText(/^stinv_/).textContent();
  fs.writeFileSync(`${OUT}/invite-link.txt`, link ?? "");
  console.log(`  invite link: ${link?.slice(0, 24)}…`);
  await shot(page, "08-invite", "the invitation link, shown once");

  // Which of "emailed" and "send it yourself" happened has to be on the
  // screen: an admin who cannot tell will send it twice or not at all.
  const emailed = await page
    .getByText(/^Emailed\./)
    .isVisible()
    .catch(() => false);
  if (!emailed) {
    const note = await page
      .getByText(/not shown again/)
      .first()
      .textContent()
      .catch(() => "");
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `the invitation was not emailed (panel says: ${note?.trim()}) — is STRATUM_MAIL_TRANSPORT set?`,
    });
  }
});

await step("the invitation as the recipient sees it", async () => {
  if (!MAIL_DIR) {
    problems.push({
      where: stage,
      kind: "harness",
      text: "STRATUM_MAIL_DIR is not set, so the message itself was never read — set it to the stack's capture directory",
    });
    return;
  }
  // The transport writes inside the request that triggered it, so this
  // is a short wait for a file, not a poll for a background job.
  let mail = null;
  for (let i = 0; i < 50 && !mail; i++) {
    for (const name of fs.readdirSync(MAIL_DIR)) {
      if (!name.endsWith(".json")) continue;
      const m = JSON.parse(fs.readFileSync(path.join(MAIL_DIR, name), "utf8"));
      if (m.to === NEWHIRE) mail = m;
    }
    if (!mail) await new Promise((r) => setTimeout(r, 100));
  }
  if (!mail) {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `no message was captured for ${NEWHIRE}`,
    });
    return;
  }
  console.log(`  subject: ${mail.subject}`);
  fs.writeFileSync(
    `${OUT}/invite-mail.txt`,
    `To: ${mail.to}\nSubject: ${mail.subject}\n\n${mail.text}`,
  );
  const found = mail.text.match(/https?:\/\/\S+/);
  if (!found) {
    problems.push({
      where: stage,
      kind: "content",
      text: "the invitation message contains no link",
    });
    return;
  }
  emailedLink = found[0];
  console.log(`  emailed link: ${emailedLink}`);
  if (!emailedLink.startsWith(BASE)) {
    problems.push({
      where: stage,
      kind: "content",
      text: `the emailed link points somewhere else: ${emailedLink}`,
    });
  }
});

await step("joining from the emailed link", async () => {
  if (!emailedLink) return;
  // A separate context: this is a different person, on a different
  // machine, with no session — which is the only way to find out
  // whether the link works on its own.
  const joinerCtx = await browser.newContext({
    viewport: { width: 1440, height: 900 },
  });
  const joiner = await joinerCtx.newPage();
  watch(joiner, () => stage);
  await joiner.goto(emailedLink, { waitUntil: "networkidle" });
  // The screen has to say what is being joined before it asks for a
  // password. A heading that only says "accept your invitation" asks a
  // stranger for a credential in exchange for nothing they can check.
  await joiner
    .getByRole("heading", { name: "Join acme" })
    .waitFor({ timeout: 10000 });
  if (
    !(await joiner.getByText(`${NEWHIRE} was invited as member`).isVisible())
  ) {
    problems.push({
      where: stage,
      kind: "content",
      text: "the accept screen does not name the address and role it was sent for",
    });
  }
  await shot(
    joiner,
    "08b-accept-invite",
    "the screen the emailed link lands on — it names the org, the address and the role",
  );
  await joiner.getByLabel("Your name").fill("New Hire");
  await joiner.getByLabel("Password").fill("a long enough password");
  await joiner.getByRole("button", { name: "Accept invitation" }).click();
  await joiner.getByText("Requests today").waitFor({ timeout: 15000 });
  await shot(joiner, "08c-joined", "joined, and signed straight in");
  if (new URL(joiner.url()).hash !== "") {
    problems.push({
      where: stage,
      kind: "security",
      text: `the invitation token is still in the address bar: ${joiner.url()}`,
    });
  }
  await audit(joiner, stage);
  await joinerCtx.close();

  // Put the org back as it was found: the new member is removed again.
  await page.reload({ waitUntil: "networkidle" });
  await page.getByRole("link", { name: "Members" }).click();
  await page.getByText(NEWHIRE).waitFor({ timeout: 10000 });
  await page.getByLabel(`Remove ${NEWHIRE}`).click();
  await page
    .getByRole("alertdialog")
    .getByRole("button", { name: "Remove" })
    .click();
  // Wait on the observable — the roster row leaving — rather than a
  // fixed delay, and look in the table: the success toast also names
  // the address, so a page-wide text match would find the toast and
  // report a removal that in fact worked.
  const gone = await page
    .locator("tbody tr", { hasText: NEWHIRE })
    .first()
    .waitFor({ state: "detached", timeout: 10000 })
    .then(() => true)
    .catch(() => false);
  if (!gone) {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `${NEWHIRE} survived removal`,
    });
  }
});

// **Two labels, not one.** These name two different objects — a personal
// token and an SSH key — and reusing one string for both made an
// assertion about the key satisfiable by a leftover toast about the
// token: revoking the token raises "Revoked <label>", and the next
// stage's page-wide `getByText(<label>)` matched that div as well as the
// key's own row. Strict mode caught it as a throw, but the more
// dangerous half is that it could have gone the other way: had the key
// never been registered at all, the toast alone would have satisfied the
// wait and the stage would have reported success.
const RUN_LABEL = `walkthrough-token-${Date.now()}`;
const KEY_LABEL = `walkthrough-key-${Date.now()}`;

await step("settings / tokens", async () => {
  await page.getByRole("link", { name: "Tokens" }).click();
  // A label unique to this run, so the row we revoke below is the token
  // we just minted and not one an earlier run left behind.
  await page.getByLabel("Token label").fill(RUN_LABEL);
  await page.getByRole("button", { name: "Mint token" }).click();
  await page.getByText(/^weft_/).waitFor({ timeout: 10000 });
  const tok = await page.getByText(/^weft_/).textContent();
  fs.writeFileSync(`${OUT}/minted-token.txt`, tok ?? "");
  console.log(`  minted: ${tok?.slice(0, 20)}…`);
  await shot(page, "09-tokens", "a personal token, plaintext shown once");

  // The token has to actually work; a plaintext string that authenticates
  // nothing would look identical on screen.
  const status = await page.evaluate(async (t) => {
    const r = await fetch("/v1/orgs/acme/repos?limit=1", {
      headers: { Authorization: `Bearer ${t}` },
    });
    return r.status;
  }, tok?.trim());
  console.log(`  the minted token authenticates: ${status}`);
  if (status !== 200)
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `minted token answered ${status}`,
    });

  // Revoke it, and confirm it is dead on the very next request.
  // The row for *this* token: several older walkthrough runs may be
  // listed, and a revoked one has no Revoke button to click.
  const row = page.locator("tr", { hasText: RUN_LABEL }).first();
  await row.getByRole("button", { name: "Revoke" }).click();
  await page
    .getByRole("alertdialog")
    .getByRole("button", { name: "Revoke" })
    .click();
  await page.waitForTimeout(700);
  const after = await page.evaluate(async (t) => {
    const r = await fetch("/v1/orgs/acme/repos?limit=1", {
      headers: { Authorization: `Bearer ${t}` },
    });
    return r.status;
  }, tok?.trim());
  console.log(`  after revocation: ${after}`);
  if (after === 200)
    problems.push({
      where: stage,
      kind: "security",
      text: "a revoked token still works",
    });
});

await step("settings / ssh keys", async () => {
  await page.getByRole("link", { name: "SSH keys" }).click();
  await page.getByLabel("Public key").fill(PUBKEY);
  await page.getByLabel("Key label").fill(KEY_LABEL);
  await page.getByRole("button", { name: "Add key" }).click();
  // Scoped to the keys table, the way the token stage already scopes
  // its own row. A page-wide text match is satisfied by any banner or
  // toast that happens to quote the label.
  await page
    .locator("tr", { hasText: KEY_LABEL })
    .first()
    .waitFor({ timeout: 10000 });
  await shot(page, "10-sshkeys", "a personal key — no token id anywhere");

  console.log("  key registered");
});

/// The key we just registered has to actually clone something.
///
/// Registering a key and seeing it listed proves the form works, not
/// that the key does. This copies the SSH URL out of the repo screen —
/// the exact string a person would take — and clones with it, then
/// revokes the key and proves the very next clone is refused.
/// Open a repository from the dashboard's list.
///
/// The row is a link now, and it goes to the repository's one address —
/// `/{owner}/{repo}` — rather than to a dashboard-only screen that had no
/// address at all. Every stage below that used to click a row and land on
/// that screen goes through here and then through `repoTab`.
async function openRepo(name) {
  // From the dashboard, always — not from wherever the last stage
  // happened to finish. The row is on the dashboard's list and the list
  // is behind the left rail, which does not exist on the forge mount, so
  // a helper that assumed "we are on the dashboard" worked only by the
  // accident of stage ordering. Two stages have already thrown on that,
  // and a thrown stage takes every stage after it with it.
  await page.goto(`${BASE}/dashboard/`, { waitUntil: "networkidle" });
  await page.getByRole("link", { name, exact: true }).click();
  await page.waitForURL(new RegExp(`/${name}$`), { timeout: 15000 });
}

/// A tab on the repository page. Links, not `role="tab"` — a tab is an
/// address somebody can send.
function repoTab(name) {
  return page
    .getByRole("navigation", { name: "Repository" })
    .getByRole("link", { name, exact: true });
}

await step("clone over ssh", async () => {
  await openRepo("widget");
  // Behind the Code button, on the repository's own page, where a
  // stranger reading a public repository can reach it too — it used to
  // be on a dashboard screen behind the sign-in.
  await page.getByRole("button", { name: "Code" }).click();
  const sshToggle = page.getByRole("button", { name: "SSH", exact: true });
  if (!(await sshToggle.count())) {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "the repo page offers no SSH clone URL — is STRATUM_SSH_PUBLIC_URL set?",
    });
    return;
  }
  await sshToggle.click();
  const sshField = page.getByRole("textbox", { name: "SSH clone URL" });
  if (!(await sshField.count())) {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "the SSH toggle is offered but shows no URL",
    });
    return;
  }
  const sshUrl = await sshField.inputValue();
  console.log(`  the screen offers: ${sshUrl}`);
  await shot(page, "11-clone-urls", "both clone URLs, copyable");

  const dest = `${KEYDIR}/cloned`;
  fs.rmSync(dest, { recursive: true, force: true });
  const gitSsh = `ssh -i ${KEYDIR}/id -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null`;
  try {
    execFileSync("git", ["clone", "--quiet", sshUrl, dest], {
      env: {
        ...process.env,
        GIT_SSH_COMMAND: gitSsh,
        GIT_TERMINAL_PROMPT: "0",
      },
      stdio: "pipe",
      timeout: 60000,
    });
    // A clone that produces a broken repository is not a clone.
    execFileSync("git", ["-C", dest, "fsck", "--full", "--strict"], {
      stdio: "pipe",
    });
    const head = execFileSync("git", ["-C", dest, "log", "--oneline", "-1"], {
      encoding: "utf8",
    }).trim();
    console.log(`  cloned over ssh and fsck'd: ${head}`);
  } catch (e) {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `cloning with the registered key failed: ${String(e.stderr ?? e).slice(0, 300)}`,
    });
  }

  // Revoke it, and the very next clone must be refused. Revocation that
  // only removes a row from a table is not revocation.
  //
  // Back to the dashboard first. The clone URL now comes off the
  // repository's own page, which is on the *forge* mount — a global
  // header and no left rail — so "SSH keys" is not on screen the way it
  // was when this whole stage ran inside `/dashboard`. Without this the
  // stage throws here, and every stage after it runs against whatever
  // state that left: 14 of them, in the run that found this.
  await page.goto(`${BASE}/dashboard/settings/ssh-keys`, { waitUntil: "networkidle" });
  await page.getByRole("link", { name: "SSH keys" }).click();
  const row = page.locator("tr", { hasText: KEY_LABEL }).first();
  await row.getByRole("button", { name: "Revoke" }).click();
  await page
    .getByRole("alertdialog")
    .getByRole("button", { name: "Revoke" })
    .click();
  await page
    .locator("tr", { hasText: KEY_LABEL })
    .waitFor({ state: "detached", timeout: 10000 });
  await page.getByRole("button", { name: /Show \d+ revoked/ }).click();
  await page
    .locator("tr", { hasText: KEY_LABEL })
    .first()
    .getByText("revoked")
    .waitFor({ timeout: 10000 });

  fs.rmSync(dest, { recursive: true, force: true });
  let refused = false;
  try {
    execFileSync("git", ["clone", "--quiet", sshUrl, dest], {
      env: {
        ...process.env,
        GIT_SSH_COMMAND: gitSsh,
        GIT_TERMINAL_PROMPT: "0",
      },
      stdio: "pipe",
      timeout: 60000,
    });
  } catch {
    refused = true;
  }
  console.log(`  after revocation the same key is refused: ${refused}`);
  if (!refused)
    problems.push({
      where: stage,
      kind: "security",
      text: "a revoked SSH key still clones",
    });
});

await step("settings / activity", async () => {
  await page.getByRole("link", { name: "Activity" }).click();
  await page.locator("tbody tr").first().waitFor({ timeout: 10000 });

  // Newest first, against a real trail. Everything this run just did —
  // mint, revoke, add a key, revoke it — happened seconds ago, so if the
  // feed is reading from the wrong end none of it is on the first page.
  const top = await page.locator("tbody tr").first().innerText();
  console.log(`  newest row: ${top.replace(/\s+/g, " ").slice(0, 90)}`);
  const first = await page
    .locator("tbody tr")
    .first()
    .locator("td")
    .nth(2)
    .innerText();
  if (!/^sshkey\.|^token\./.test(first.trim()))
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `the feed's newest row is ${first.trim()}, not this run's work — reading from the wrong end?`,
    });

  // Filtering by repo narrows it to that repo's own work, and the column
  // has to name the repo rather than echo the id back.
  await page.getByLabel("Filter by repo").click();
  await page.getByRole("option", { name: "widget" }).click();
  await page.waitForTimeout(700);
  const repoRows = await page.locator("tbody tr").count();
  console.log(`  widget rows: ${repoRows}`);
  if (repoRows === 0)
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "filtering by repo widget found nothing",
    });
  const repoCell = await page
    .locator("tbody tr")
    .first()
    .locator("td")
    .nth(3)
    .innerText();
  if (repoCell.trim() !== "widget")
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `the repo column says ${repoCell.trim()}, not the repo that was filtered for`,
    });
  await page.getByLabel("Filter by repo").click();
  await page.getByRole("option", { name: "any" }).click();
  await page.waitForTimeout(500);

  // Filtering narrows it. `token.mint` is one of the things this run did.
  await page.getByLabel("Filter by action").fill("token.mint");
  await page.waitForTimeout(700);
  const minted = await page.locator("tbody tr").count();
  console.log(`  token.mint rows: ${minted}`);
  if (minted === 0)
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "filtering by token.mint found nothing",
    });

  // An action nobody has ever taken is an empty state, not an error.
  await page.getByLabel("Filter by action").fill("no.such.action");
  await page
    .getByText("Nothing matches those filters.")
    .waitFor({ timeout: 10000 });
  await page.getByLabel("Filter by action").fill("");
  await page.locator("tbody tr").first().waitFor({ timeout: 10000 });
  await shot(page, "12-activity", "the trail, newest first, with filters");

  // The export must arrive as a real file with real rows. A plain
  // download link would come back as a 401 page named `.csv`.
  const dl = page.waitForEvent("download", { timeout: 15000 });
  await page.getByRole("button", { name: "Export CSV" }).click();
  const file = await dl;
  const csvPath = `${OUT}/${file.suggestedFilename()}`;
  await file.saveAs(csvPath);
  const csv = fs.readFileSync(csvPath, "utf8");
  const lines = csv.trim().split("\n");
  console.log(
    `  exported ${file.suggestedFilename()}: ${lines.length - 1} rows`,
  );
  if (lines[0] !== "seq,at,action,principal,user_email,repo_id,context")
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `csv header is ${lines[0]}`,
    });
  if (lines.length < 2)
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "the csv export has no rows",
    });
  // Every row must carry the same number of cells as the header, or the
  // quoting has let a context blob's commas split a line.
  const cells = (line) => line.match(/("([^"]|"")*"|[^,]*)(,|$)/g)?.length ?? 0;
  const width = cells(lines[0]);
  const ragged = lines.slice(1).filter((l) => cells(l) !== width);
  if (ragged.length)
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `${ragged.length} csv row(s) split wrong, e.g. ${ragged[0].slice(0, 120)}`,
    });
});

await step("settings / teams", async () => {
  await page.getByRole("link", { name: "Teams" }).click();
  const team = `squad-${Date.now()}`;
  await page.getByLabel("Team name").fill(team);
  await page.getByLabel("Team description").fill("made by the walkthrough");
  await page.getByRole("button", { name: "Create team" }).click();
  // Creating selects it, so its (empty) roster opens straight away.
  await page
    .getByText("Nobody is in this team yet.")
    .waitFor({ timeout: 10000 });

  // Staff it from the org roster. The control must not offer somebody
  // who is already in the team.
  await page.getByLabel("Add to team").click();
  await page.getByRole("option", { name: "dev@acme.dev" }).click();
  await page.getByRole("button", { name: "Add", exact: true }).click();
  await page
    .getByRole("cell", { name: "dev@acme.dev", exact: true })
    .waitFor({ timeout: 10000 });
  const stillOffered = await page
    .getByLabel("Add to team")
    .locator("option")
    .filter({ hasText: "dev@acme.dev" })
    .count();
  if (stillOffered)
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "somebody already in the team is still offered to add",
    });
  await shot(page, "13-teams", "a team, and who is in it");

  // The team is what the repo Access panel will grant to, so leave it in
  // place and record its name for that step.
  fs.writeFileSync(`${OUT}/team.txt`, team);
  console.log(`  team ${team} created and staffed`);
});

await step("repo access", async () => {
  const team = fs.readFileSync(`${OUT}/team.txt`, "utf8");
  await openRepo("widget");
  await repoTab("Settings").click();
  await page.getByText("Access", { exact: true }).waitFor({ timeout: 10000 });

  // Everyone with an org role shows up, and says so.
  const owner = page.locator("tr", { hasText: "ada@acme.dev" }).first();
  const from = (await owner.locator("td").nth(2).innerText()).trim();
  console.log(`  the owner's access comes from: ${from}`);
  if (from !== "org role")
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `owner's access reads "${from}"`,
    });
  // …and there is nothing to revoke on a role that was not granted here.
  if (await owner.getByRole("button", { name: "Revoke" }).count())
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "an org role is offered a Revoke button, which would do nothing",
    });

  // Grant the team, and watch it land as its own row.
  await page.getByLabel("Grant access to").click();
  await page.getByRole("option", { name: `${team} (team)` }).click();
  // Above dev@acme.dev's org role, so the grant genuinely raises them —
  // a team grant equal to the org role adds nothing and the panel is
  // right to keep saying "org role".
  await page.getByLabel("Role to grant").click();
  await page.getByRole("option", { name: "admin" }).click();
  await page.getByRole("button", { name: "Grant", exact: true }).click();
  const teamRow = page.locator("tr", { hasText: team }).first();
  await teamRow.waitFor({ timeout: 10000 });
  await teamRow.getByText("admin").waitFor({ timeout: 10000 });

  // The person in that team is now reached through it, and the panel
  // says which team — the whole reason this screen exists.
  const dev = page.locator("tr", { hasText: "dev@acme.dev" }).first();
  const devFrom = (await dev.locator("td").nth(2).innerText()).trim();
  console.log(`  dev@acme.dev now reaches it via: ${devFrom}`);
  if (!devFrom.includes("team") || !devFrom.includes(team))
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `a team-granted person reads "${devFrom}", not the team that granted it`,
    });
  await shot(page, "14-access", "who can reach this repo, and why");

  // Withdraw it again. The team's own row goes, and so does the mention
  // of it in the row of the person it was raising — two rows carry the
  // name, so this waits for both to stop doing so.
  await teamRow.getByRole("button", { name: "Revoke" }).click();
  await expect
    .poll(async () => page.locator("tr", { hasText: team }).count(), {
      timeout: 10000,
    })
    .toBe(0);
  console.log("  team grant withdrawn");

  // Leave the org as this run found it — the same discipline as the
  // token and key steps. Runs accumulate otherwise, and a Teams screen
  // full of `squad-1787…` is nobody's idea of a clean walkthrough.
  //
  // Back to the dashboard first, for the reason the ssh stage gives: the
  // access map is on the repository's Settings tab now, which is the
  // *forge* mount, and the left rail with "Teams" on it is not there.
  await page.goto(`${BASE}/dashboard/settings/teams`, { waitUntil: "networkidle" });
  await page.getByRole("link", { name: "Teams" }).click();
  await page
    .locator("tr", { hasText: team })
    .getByRole("button", { name: "Delete" })
    .click();
  await page
    .getByRole("alertdialog")
    .getByRole("button", { name: "Delete" })
    .click();
  await expect
    .poll(async () => page.locator("tr", { hasText: team }).count(), {
      timeout: 10000,
    })
    .toBe(0);
  console.log("  team deleted");
});

// ---- Changes: OWNERS-governed review, exercised for real ----
//
// Seed through the same API the session cookie authorizes: an OWNERS
// file on trunk, a review branch, one commit carrying a Change-Id
// trailer. Then drive the review in the UI — register, watch the
// verdict block, approve, land — and prove the landing by reading the
// file back from trunk. A fresh Change-Id per run keeps reruns clean:
// a landed change is terminal, and colliding with last run's would be
// a 409, not a review.
const CHANGE_KEY = `I${Date.now().toString(16).padStart(12, '0')}`;
const REVIEW_BRANCH = `review-${Date.now()}`;
const REVIEW_FILE = `fees/schedule-${Date.now()}.txt`;

/// Approve the latest patchset through the review sheet.
///
/// Approving *is* a review now: the standalone Approve button only
/// renders against a server older than migration 0051, so a walkthrough
/// that still clicks it is testing the degraded path on a modern server —
/// or, as it did here, timing out on a control that is not there. This
/// is the local gate falling behind the product, which is worse than no
/// gate, because it is trusted.
async function approveThroughSheet(page, timeout = 20000) {
  await page.getByRole('button', { name: /^Review patchset/ }).waitFor({ timeout });
  await page.getByRole('button', { name: /^Review patchset/ }).click();
  const sheet = page.getByRole('region', { name: 'Submit your review' });
  await sheet.waitFor({ timeout: 10000 });
  await sheet.getByRole('radio', { name: 'Approve', exact: true }).check();
  await sheet.getByRole('button', { name: 'Submit review' }).click();
  // The sheet closes when the verdict is recorded; waiting on that rather
  // than on the click keeps the next step from racing the round trip.
  await sheet.waitFor({ state: 'detached', timeout });
}

await step('changes / seed', async () => {
  const out = await page.evaluate(
    async ({ key, branch, file }) => {
      const post = async (path, body) => {
        const r = await fetch(path, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(body),
        });
        return { status: r.status, body: await r.json().catch(() => null) };
      };
      const owners = await post('/v1/orgs/acme/repos/payments-api/commits', {
        branch: 'main',
        message: 'fees need a second pair of eyes',
        operations: [{ op: 'put', path: 'fees/OWNERS', content: 'ada@acme.dev\n' }],
      });
      if (owners.status !== 201) return `OWNERS commit answered ${owners.status}`;
      const br = await post('/v1/orgs/acme/repos/payments-api/branches', {
        name: branch,
        from: 'main',
      });
      if (br.status !== 201) return `branch create answered ${br.status}`;
      const work = await post('/v1/orgs/acme/repos/payments-api/commits', {
        branch,
        message: `adjust the fee schedule\n\nChange-Id: ${key}\n`,
        operations: [{ op: 'put', path: file, content: 'flat 0.1%\n' }],
      });
      if (work.status !== 201) return `review commit answered ${work.status}`;
      return 'ok';
    },
    { key: CHANGE_KEY, branch: REVIEW_BRANCH, file: REVIEW_FILE },
  );
  if (out !== 'ok') problems.push({ where: stage, kind: 'behaviour', text: out });
});

await step('changes / protect trunk', async () => {
  // Branch policy is a repository setting, and lives with the rest of
  // them. Asked for by address: the repository has one.
  await page.goto(`${BASE}/acme/payments-api/settings`, { waitUntil: 'networkidle' });
  await page.getByText('Branch policy').waitFor({ timeout: 10000 });
  await page.getByLabel('Branch to protect').fill('main');
  await page.getByRole('button', { name: 'Protect', exact: true }).click();
  await page.getByText('lands through review only').waitFor({ timeout: 10000 });
  await shot(page, '14b-branch-policy', 'trunk protected: review is now the only road');
  // Exercise the fence, not the form: a direct commit must come back
  // with the same sentence every other door uses.
  const refusal = await page.evaluate(async () => {
    const r = await fetch('/v1/orgs/acme/repos/payments-api/commits', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        branch: 'main',
        message: 'sneak past review',
        operations: [{ op: 'put', path: 'sneak.txt', content: 'x' }],
      }),
    });
    return { status: r.status, body: await r.json().catch(() => null) };
  });
  if (
    refusal.status !== 403 ||
    refusal.body?.error !== "branch 'main' is protected: land through review"
  )
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `direct commit to protected trunk answered ${refusal.status}: ${JSON.stringify(refusal.body)}`,
    });
  console.log(`  direct commit refused: ${refusal.body?.error}`);
});

await step('changes / review blocked until an owner approves', async () => {
  await repoTab('Changes').click();
  await page.getByLabel('Branch to review').fill(REVIEW_BRANCH);
  await page.getByRole('button', { name: 'Start review' }).click();
  // The detail opens on the fresh change, blocked with the reason.
  await page.getByText('■ Blocked').waitFor({ timeout: 10000 });
  const explanation = await page
    .getByText(/blocked: needs an owner of/)
    .first()
    .textContent();
  console.log(`  verdict: ${explanation}`);
  if (!explanation?.includes('fees/'))
    problems.push({
      where: stage,
      kind: 'content',
      text: `the blocked verdict does not name the governed path: ${explanation}`,
    });
  const land = page.getByRole('button', { name: 'Land on main' });
  if (await land.isEnabled())
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: 'the Land button is enabled while the verdict blocks',
    });
  await shot(page, '15-change-blocked', 'a change blocked by OWNERS, with the reason in words');
});

await step('changes / the page names who it is waiting on', async () => {
  // The engine has always known this: `required_reviewers` resolves the
  // OWNERS files on the target branch, through teams, to actual people.
  // For a long time its only reader was the mail worker, so the product
  // could tell you a change was blocked and never tell you who could
  // unblock it.
  const card = page.getByRole('region', { name: 'Required reviewers' });
  await card.waitFor({ timeout: 10000 });
  const names = await card.textContent();
  console.log(`  waiting on: ${names?.replace(/\s+/g, ' ').trim().slice(0, 120)}`);

  // The claim the card makes is that nobody nominated this list. If it
  // ever grows a control that changes who is required, that is the
  // product losing the argument it is built on.
  for (const forbidden of ['Request a review', 'Add reviewer', 'Request review']) {
    if ((await card.getByText(forbidden, { exact: false }).count()) > 0)
      problems.push({
        where: stage,
        kind: 'behaviour',
        text: `the reviewer card offers "${forbidden}" — the set is computed, not nominated`,
      });
  }

  // This card renders only when the page knows who is looking, and the
  // dashboard mount did not pass that until recently: "Revoke my
  // approval" sat disabled for the one person it exists for. A stage
  // here guards that mount, since the forge one was always correct.
  if (!/waiting|approved/i.test(names ?? ''))
    problems.push({
      where: stage,
      kind: 'content',
      text: `the reviewer card names nobody and no standing: ${names}`,
    });
  await shot(page, '15a-change-reviewers', 'who OWNERS requires, and who has approved so far');
});

await step('changes / read the diff in place', async () => {
  // A reviewer must be able to read what they approve: expand the file
  // and see the actual content this patchset adds.
  await page.getByRole('button', { name: new RegExp(REVIEW_FILE.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')) }).click();
  await page.getByText('flat 0.1%').waitFor({ timeout: 15000 });
  // Review at line resolution: comment on the exact line, watch the
  // thread land under it and the anchor appear in the conversation.
  // The comment control rides in the gutter beside the line under the
  // pointer, so the line is hovered first — a person does the same
  // without noticing.
  await page.getByText('flat 0.1%').first().hover();
  await page.getByRole('button', { name: 'Comment on line 1' }).click();
  await page
    .getByLabel(`Comment on ${REVIEW_FILE} line 1`)
    .fill('is 0.1% before or after rounding?');
  await page.getByRole('button', { name: 'Post line comment' }).click();
  await page
    .getByText('is 0.1% before or after rounding?')
    .first()
    .waitFor({ timeout: 10000 });
  await page.getByText(`${REVIEW_FILE}:1`).waitFor({ timeout: 10000 });
  console.log('  line comment anchored and echoed in the conversation');
  await shot(page, '15b-change-diff', 'the line diff, with a comment on the exact line');
});

await step('changes / the diff carries its own numbers and addresses', async () => {
  // Two things a reviewer uses constantly and neither had until now: how
  // much a file actually changed, before opening it, and a line you can
  // send somebody.
  //
  // The seeded review file is one line long, so there is deliberately no
  // hunk-expansion assertion here — a file with no hidden context cannot
  // exercise it, and a stage that cannot fail is worse than no stage.
  // Folding is the diff library's now, exercised by `review.spec.ts`
  // against a file with context to hide.
  const stat = page.getByText(/\d+ added, \d+ removed/).first();
  if ((await stat.count()) === 0)
    problems.push({
      where: stage,
      kind: 'content',
      text: 'a changed file shows no added/removed counts',
    });
  else console.log(`  ${(await stat.textContent())?.trim()}`);

  // A line number is an address, so a review comment can point at code
  // rather than describe where it is. Clicking one puts the anchor in
  // the address bar — that is the whole feature, and it is only real if
  // the URL changes. The number is the diff library's, inside its shadow
  // root, reached by the one data attribute this product reads from it;
  // in unified layout a context line carries two (old, then new) and
  // the new-side one is the address, so the last is the one clicked.
  const anchor = page.locator('diffs-container [data-column-number="1"]').last();
  if ((await anchor.count()) > 0) {
    await anchor.click();
    await expect
      .poll(async () => page.url(), { timeout: 5000 })
      .toMatch(/#.+:L1$/);
    console.log(`  line anchor: ${new URL(page.url()).hash}`);
  } else {
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: 'diff line numbers are not addressable — no link on line 1',
    });
  }
});

await step('changes / the conversation', async () => {
  await page.getByLabel('Comment on this change').fill('fees need a second sign-off — checking the rounding first');
  await page.getByRole('button', { name: 'Comment', exact: true }).click();
  await page.getByText('fees need a second sign-off — checking the rounding first').waitFor({ timeout: 10000 });
  // Attributed to the person who said it, against the patchset they read.
  const attributed = await page.getByText('Ada', { exact: false }).first().isVisible();
  if (!attributed)
    problems.push({ where: stage, kind: 'content', text: 'the comment is not attributed to its author' });
  await shot(page, '15c-change-conversation', 'a comment, attributed, pinned to its patchset');
});

await step('changes / a thread is answered and settled', async () => {
  // The conversation stage above left one remark. Reply to it, watch the
  // reply nest under its root rather than land as a third paragraph, and
  // settle the thread.
  //
  // This runs against the real server, so it is the only place the
  // *permission* is exercised for real: who may resolve is an OWNERS
  // question the server answers, and every spec that covers it does so
  // against a mock that was told what to say.
  const reply = page.getByRole('button', { name: /^Reply to / }).first();
  await reply.waitFor({ timeout: 10000 });
  await reply.click();
  await page.getByRole('textbox', { name: /^Reply to / }).fill('after rounding — I will pin it in a test');
  await page.getByRole('button', { name: 'Post reply' }).click();
  // Twice on the page by design — under its line in the diff and in the
  // conversation panel — so `.first()`: a strict match here reported a
  // thread that had landed exactly where it should as a thrown stage.
  await page
    .getByText('after rounding — I will pin it in a test')
    .first()
    .waitFor({ timeout: 10000 });

  // A reply must not offer its own Reply: threads here are one level, and
  // a second level is a forum we deliberately do not build.
  //
  // Scoped to the card holding the reply, not the page. Counting
  // page-wide said "2 reply buttons on one thread" and read like a
  // product bug — but this change carries two threads, a line remark and
  // a conversation remark, so two is correct and the assertion was the
  // thing that was wrong. A manual-pass assertion that cries wolf costs
  // more than a missing one: the next person learns to skim the report.
  const card = page
    .locator('li, div')
    .filter({ hasText: 'after rounding — I will pin it in a test' })
    .last();
  const replies = await card.getByRole('button', { name: /^Reply to / }).count();
  if (replies > 1)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `a reply offers its own Reply control (${replies} in one thread)`,
    });

  const settle = page.getByRole('button', { name: /^Resolve thread from / }).first();
  const beforeText = await page.getByText(/unresolved/).first().textContent();
  await settle.click();
  await expect
    .poll(async () => (await page.getByText(/unresolved/).first().textContent()) ?? '', { timeout: 10000 })
    .not.toBe(beforeText);
  console.log(`  unresolved went from "${beforeText?.trim()}" to settled`);

  // The count is a fact, not a gate. Landing is decided by checks and the
  // verdict; settling a thread must not move it either way.
  const land = page.getByRole('button', { name: 'Land on main' });
  if (await land.isEnabled())
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: 'resolving a thread made the change landable — resolution is not a gate',
    });
  await shot(page, '15c2-change-thread', 'a reply nested under its root, and the thread settled');
});

await step('changes / a suggestion is applied as a patchset', async () => {
  // A reviewer says the change in code rather than in prose, and the
  // author takes it. This runs against the real server, so it is the only
  // place the whole path is exercised: the block is parsed out of the
  // comment body, the file is fetched at the patchset the comment was
  // written against, and applying it makes a real commit through the same
  // door a push uses.
  // Open the gutter box first: the diff stage above posted its remark and
  // closed it again, so the textarea is not on the page until asked for.
  // The control appears beside the hovered line.
  await page.getByText('flat 0.1%').first().hover();
  await page
    .getByRole('button', { name: 'Comment on line 1', exact: true })
    .first()
    .click();
  await page
    .getByLabel(`Comment on ${REVIEW_FILE} line 1`)
    .fill('say it as a number:\n```suggestion\nflat 0.10%\n```');
  await page.getByRole('button', { name: 'Post line comment' }).click();

  // Rendered as a diff, not as a code fence: the point is that a reader
  // sees what would change, not that a reviewer typed a fenced block.
  const applyOne = page.getByRole('button', { name: /^Apply .*suggestion/ }).first();
  await applyOne.waitFor({ timeout: 10000 });
  await page.getByText('flat 0.10%').first().waitFor({ timeout: 10000 });

  const before = await page.getByText(/patchset 1/i).count();
  await applyOne.click();

  // The observable is a new patchset, not the button settling: a
  // suggestion that "applied" without producing one would leave the
  // author believing their file changed when nothing was committed.
  await expect
    .poll(async () => page.getByText(/patchset 2/i).count(), { timeout: 20000 })
    .toBeGreaterThan(0);
  console.log(`  suggestion applied; patchset 1 mentions before: ${before}, patchset 2 now present`);
  await shot(page, '15c3-suggestion-applied', 'a suggestion, and the patchset it became');
});

await step('changes / ci reports checks', async () => {
  // CI hears about work over webhooks and reports back through the
  // checks API; here the report is driven directly so the walkthrough
  // stays hermetic. Red first: the machine's verdict must block even
  // though an owner is about to approve.
  const red = await page.evaluate(async ({ key }) => {
    const r = await fetch(`/v1/orgs/acme/repos/payments-api/changes/${key}/checks`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name: 'ci/tests', state: 'failing' }),
    });
    return r.status;
  }, { key: CHANGE_KEY });
  if (red !== 201)
    problems.push({ where: stage, kind: 'behaviour', text: `posting a failing check answered ${red}` });
  // Re-open the change so the view reloads the checks.
  await page.getByRole('button', { name: '← Changes' }).click();
  await page.getByRole('button', { name: CHANGE_KEY }).click();
  // `.first()`: the name appears twice on a blocked change — once in the
  // Checks panel row and once inside "1 check failed: ci/tests" beside
  // the disabled button. Two matches is the page working; a strict-mode
  // violation here would be the walkthrough complaining about it.
  await page.getByText('ci/tests').first().waitFor({ timeout: 10000 });
  // "Failing", capitalised: `ChangeChecksPanel` renders
  // `checkStatePresentation(state).label`, not the wire word. Waiting on
  // the lowercase wire value passed only for as long as the panel echoed
  // it back raw.
  await page.getByText('Failing', { exact: true }).first().waitFor({ timeout: 10000 });
  // The reason, in words, beside the disabled button.
  //
  // This used to wait for "landing is blocked while a check is failing",
  // which the change view stopped rendering in 0ceb3b4 when
  // `LandBlockers`/`checkBlockers` replaced the single sentence with a
  // per-cause list. Nothing noticed, because `step()` does not catch:
  // the wait threw, the run died here, and **every stage below this one
  // stopped running** — approve-and-land, the viewer's view, mirroring,
  // browsing, the public pages, the Checks tab. A gate that reports
  // nothing after its tenth stage is worse than no gate, because the
  // summary still prints. Matched as a pattern rather than a sentence so
  // that a rewording of the count or the list does not silently truncate
  // the pass again.
  await page
    .getByText(/checks? failed: ci\/tests/)
    .waitFor({ timeout: 10000 });
  const land = page.getByRole('button', { name: 'Land on main' });
  if (await land.isEnabled())
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: 'the Land button is enabled while a check is failing',
    });
  await shot(page, '15d-change-checks', "the machine's verdict beside the human one — red blocks");
  // CI goes green; the gate opens for the next stage.
  const green = await page.evaluate(async ({ key }) => {
    const r = await fetch(`/v1/orgs/acme/repos/payments-api/changes/${key}/checks`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name: 'ci/tests', state: 'passing', url: 'https://ci.example.com/run/1' }),
    });
    return r.status;
  }, { key: CHANGE_KEY });
  if (green !== 200)
    problems.push({ where: stage, kind: 'behaviour', text: `greening the check answered ${green}` });
  await page.getByRole('button', { name: '← Changes' }).click();
  await page.getByRole('button', { name: CHANGE_KEY }).click();
  await page.getByText('Passing', { exact: true }).first().waitFor({ timeout: 10000 });
  console.log('  ci/tests reported red, blocked landing, then went green');
});

await step('changes / approve and land', async () => {
  // ada owns fees/ per the OWNERS file this run just wrote.
  await approveThroughSheet(page);
  await page.getByText('✓', { exact: false }).first().waitFor();
  await page.getByText('Landable').waitFor({ timeout: 10000 });
  await page.getByRole('button', { name: 'Land on main' }).click();
  // The view polls the change until the queue answers; landing on this
  // stack is a fast-forward promotion and finishes in seconds.
  await page.getByText('Landed as').waitFor({ timeout: 30000 });
  await shot(page, '16-change-landed', 'approved, queued, landed — with the verdict trail');
  // Exercise the thing, not the form: trunk must now serve the file
  // the change added — and it must serve the *reviewed* text, not the
  // text originally proposed. A suggestion was applied a few stages
  // above, so what lands is `flat 0.10%`. Asserting the earlier wording
  // here would pass only if the suggestion had silently done nothing,
  // which is the failure this line is placed to catch.
  const landed = await page.evaluate(async ({ file }) => {
    const r = await fetch(
      `/v1/orgs/acme/repos/payments-api/files/${file}?at=main`,
    );
    return { status: r.status, text: await r.text() };
  }, { file: REVIEW_FILE });
  if (landed.status !== 200 || !landed.text.includes('flat 0.10%'))
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `trunk does not serve the landed file: ${landed.status}`,
    });
  console.log(`  trunk serves ${REVIEW_FILE} after landing`);
});

await step('changes / lower the fence', async () => {
  // The queue landed on protected trunk — that was the proof. Unprotect
  // through the same panel, so reruns of this walkthrough can seed
  // trunk again (leave the org as this run found it).
  await page.goto(`${BASE}/acme/payments-api/settings`, { waitUntil: 'networkidle' });
  await page.getByText('Branch policy').waitFor({ timeout: 10000 });
  await page.getByRole('button', { name: 'Unprotect main' }).click();
  await page
    .getByText('protect it to make review the only road to trunk')
    .waitFor({ timeout: 10000 });
  console.log('  protection removed; direct pushes to trunk work again');
});

// ---- Changesets: N changes across N repositories, landed as one ----
//
// Everything above landed *one* change on *one* trunk. A changeset is
// the polyrepo answer: open changes in several repositories, composed,
// reviewed as a set, landed in order, and reverted as a set if they
// have to be. Two members here — `payments-api`, whose `fees/` is
// governed by the OWNERS file the changes stages wrote, and `ledger`,
// where nothing is governed — so the composed verdict has one member
// waiting on a person and one that is not, and the gate has to say
// which. Seeded out of band the way the changes stages are; everything
// after that is the dashboard, and every screen is proved against the
// server rather than read back from itself: the workspace is really
// cloned with `--recurse-submodules`, and both trunks are really read
// after the landing.
const CS_STAMP = Date.now();
const CS_KEY = `tiered-fees-${CS_STAMP}`;
const CS_TITLE = 'Tiered fees, end to end';
const CS_FILE = `fees/tiers-${CS_STAMP}.txt`;
const CS_MEMBERS = {
  'payments-api': {
    key: `I${(CS_STAMP + 2).toString(16).padStart(12, '0')}`,
    branch: `cs-api-${CS_STAMP}`,
    title: 'tier the fee schedule',
    content: 'tiered 0.2% over 1k\n',
  },
  ledger: {
    key: `I${(CS_STAMP + 3).toString(16).padStart(12, '0')}`,
    branch: `cs-ledger-${CS_STAMP}`,
    title: 'book tiered fees',
    content: 'tiered fees, booked per tier\n',
  },
};
const CS_DIR = fs.mkdtempSync(path.join(os.tmpdir(), 'walk-changeset-'));
let csWorkspaceToken = null;

// The member row for a repository, in the changeset's Members table.
function csMemberRow(page, repo) {
  return page.locator('tr', { hasText: CS_MEMBERS[repo].key });
}

await step('changesets / seed two open changes', async () => {
  const out = await page.evaluate(
    async ({ members, file }) => {
      const post = async (path, body) => {
        const r = await fetch(path, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(body),
        });
        return { status: r.status, body: await r.json().catch(() => null) };
      };
      // `ledger` is seeded empty; a branch cannot fork from a trunk that
      // does not exist yet. Only then: rerunning against a ledger that
      // has a main must not stack scaffolds on it.
      const branches = await fetch('/v1/orgs/acme/repos/ledger/branches');
      const listed = JSON.stringify(await branches.json().catch(() => null));
      if (!listed.includes('"main"')) {
        const scaffold = await post('/v1/orgs/acme/repos/ledger/commits', {
          message: 'scaffold',
          operations: [{ op: 'put', path: 'README.md', content: '# ledger\n' }],
        });
        if (scaffold.status !== 201)
          return `ledger scaffold answered ${scaffold.status}`;
      }
      for (const [repo, m] of Object.entries(members)) {
        const br = await post(`/v1/orgs/acme/repos/${repo}/branches`, {
          name: m.branch,
          from: 'main',
        });
        if (br.status !== 201) return `${repo}: branch create answered ${br.status}`;
        const work = await post(`/v1/orgs/acme/repos/${repo}/commits`, {
          branch: m.branch,
          message: `${m.title}\n\nChange-Id: ${m.key}\n`,
          operations: [{ op: 'put', path: file, content: m.content }],
        });
        if (work.status !== 201) return `${repo}: review commit answered ${work.status}`;
        const change = await post(`/v1/orgs/acme/repos/${repo}/changes`, {
          from: m.branch,
        });
        // The answer is `{change, patchset}`, the same shape the change
        // page reads — not the change itself at the top level.
        if (change.status !== 201 || change.body?.change?.key !== m.key)
          return `${repo}: registering the change answered ${change.status}: ${JSON.stringify(change.body)}`;
      }
      return 'ok';
    },
    { members: CS_MEMBERS, file: CS_FILE },
  );
  if (out !== 'ok') problems.push({ where: stage, kind: 'behaviour', text: out });
  else console.log('  two open changes registered, one per repository');
});

await step('changesets / compose from open changes', async () => {
  // Come in from the dashboard rather than from wherever the previous
  // stage finished. `changes / lower the fence` now ends on a repository
  // *settings* page, which is the forge mount and has no left rail, so
  // the rail link below was being clicked on a page that does not have
  // one — and a stage that throws takes every stage after it with it.
  // Ten of the eleven changeset stages were failing on this one line.
  await page.goto(`${BASE}/dashboard/`, { waitUntil: 'networkidle' });
  // `exact`, because accessible-name matching is a substring by
  // default and the changeset page now carries its own
  // "← All changesets" back-link, which contains this name.
  await page.getByRole('link', { name: 'Changesets', exact: true }).click();
  await page.getByRole('heading', { name: 'Changesets' }).waitFor({ timeout: 10000 });
  await page.getByRole('button', { name: 'New changeset' }).click();
  await page.getByLabel('Title', { exact: true }).fill(CS_TITLE);
  // The key follows the title until it is edited; this run wants its
  // own, so reruns never collide with last run's landed set.
  await page.getByLabel('Key', { exact: true }).fill(CS_KEY);
  // The picker offers every open change in the org, grouped by
  // repository — and only the open ones. The change the stages above
  // landed must not be on offer: a landed change is terminal, and a
  // picker that lists it would let somebody compose a set that can
  // never land.
  const api = page.getByLabel(new RegExp(`^payments-api/${CS_MEMBERS['payments-api'].key}:`));
  const ledger = page.getByLabel(new RegExp(`^ledger/${CS_MEMBERS.ledger.key}:`));
  await api.waitFor({ timeout: 10000 });
  await ledger.waitFor({ timeout: 10000 });
  if ((await page.getByLabel(new RegExp(`^payments-api/${CHANGE_KEY}:`)).count()) !== 0)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `the picker offers ${CHANGE_KEY}, which landed two stages ago`,
    });
  const create = page.getByRole('button', { name: 'Create changeset' });
  if (await create.isEnabled())
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: 'Create changeset is enabled with no member picked',
    });
  await api.check();
  // One change per repository: with payments-api picked, its other open
  // change — the review branch the changes stages registered and then
  // landed is gone, but a rerun leaves last run's abandoned revert
  // members open — must be greyed with the reason, not silently
  // refused on submit.
  const apiBoxes = page.getByLabel(/^payments-api\//);
  for (let i = 0; i < (await apiBoxes.count()); i += 1) {
    const box = apiBoxes.nth(i);
    const name = await box.getAttribute('aria-label');
    if (name?.includes(CS_MEMBERS['payments-api'].key)) continue;
    if (await box.isEnabled())
      problems.push({
        where: stage,
        kind: 'behaviour',
        text: `${name} is still pickable beside ${CS_MEMBERS['payments-api'].key}, in the same repository`,
      });
  }
  await ledger.check();
  await page.getByText('Members 2/16').waitFor({ timeout: 5000 });
  await shot(page, '16b-changeset-compose', 'two open changes in two repositories, picked into one set');
  await create.click();
  await page.waitForURL(`**/dashboard/changesets/${CS_KEY}`, { timeout: 15000 });
  await page.getByRole('heading', { name: CS_TITLE }).waitFor({ timeout: 10000 });
  console.log(`  changeset ${CS_KEY} composed with 2 members`);
});

await step('changesets / the composed verdict names the member in the way', async () => {
  // The gate is one word for the whole set, and while an owner has not
  // approved the payments-api member it must be that member, by name,
  // that the explanation blames — not "the changeset is blocked".
  const verdict = page.locator('section', { hasText: 'Verdict' }).first();
  await verdict.getByText('blocked', { exact: true }).waitFor({ timeout: 15000 });
  const explanation = await verdict.locator('p').first().textContent();
  console.log(`  verdict: ${explanation}`);
  if (!explanation?.includes(`payments-api/${CS_MEMBERS['payments-api'].key}`))
    problems.push({
      where: stage,
      kind: 'content',
      text: `the blocked verdict does not name the member in the way: ${explanation}`,
    });
  const land = page.getByRole('button', { name: 'Land all members' });
  if (await land.isEnabled())
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: 'Land all members is enabled while a member is blocked',
    });
  // Two rows, in landing order, each with its own gate and its own
  // reason. No OWNERS rule governs the ledger file, and that does not
  // make it free: an ungoverned path needs one approval from anybody
  // with write access, and the row says exactly that.
  await csMemberRow(page, 'payments-api').waitFor({ timeout: 10000 });
  const ledgerRow = csMemberRow(page, 'ledger');
  await ledgerRow.getByText('blocked', { exact: true }).waitFor({ timeout: 10000 });
  if (!(await ledgerRow.textContent())?.includes('needs any approval with write access'))
    problems.push({
      where: stage,
      kind: 'content',
      text: `the ungoverned member does not say what it needs: ${await ledgerRow.textContent()}`,
    });
  await shot(page, '16c-changeset-blocked', 'a set blocked by both members, each row saying why');
});

await step('changesets / membership can change while it is open', async () => {
  // Remove ledger, watch the set shrink to one, add it back through the
  // picker — which must offer ledger and must not offer payments-api,
  // already a member.
  await csMemberRow(page, 'ledger')
    .getByRole('button', { name: `Remove ledger/${CS_MEMBERS.ledger.key}` })
    .click();
  await expect
    .poll(async () => csMemberRow(page, 'ledger').count(), { timeout: 10000 })
    .toBe(0);
  await page.getByRole('button', { name: 'Add member' }).click();
  const ledger = page.getByLabel(new RegExp(`^ledger/${CS_MEMBERS.ledger.key}:`));
  await ledger.waitFor({ timeout: 10000 });
  if ((await page.getByLabel(new RegExp(`^payments-api/${CS_MEMBERS['payments-api'].key}:`)).count()) !== 0)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: 'Add member offers a change that is already a member',
    });
  await ledger.check();
  await page.getByRole('button', { name: 'Add to changeset' }).click();
  await csMemberRow(page, 'ledger').waitFor({ timeout: 10000 });
  // The order is the order members were added: ledger is now last.
  const rows = await page.locator('main table tbody tr').allTextContents();
  const apiAt = rows.findIndex((r) => r.includes(CS_MEMBERS['payments-api'].key));
  const ledgerAt = rows.findIndex((r) => r.includes(CS_MEMBERS.ledger.key));
  if (apiAt === -1 || ledgerAt === -1 || apiAt > ledgerAt)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `members are not in landing order after re-adding: ${JSON.stringify(rows)}`,
    });
  console.log('  removed and re-added a member; two members again, payments-api first');
});

await step('changesets / the landing order is a picture you can correct', async () => {
  // The strip is the plan the members table renders, so the two must
  // agree. With no edges declared the section still renders and says so —
  // that sentence is the only place a reader learns the order can be
  // edited at all, which is why an empty state is asserted rather than
  // skipped.
  const strip = page.locator('section', { hasText: 'Landing order' }).first();
  await strip.waitFor({ timeout: 10000 });
  if ((await strip.getByText(/No declared dependencies/).count()) !== 1)
    problems.push({
      where: stage,
      kind: 'content',
      text: 'with no edges the strip does not say the members land in the order they were added',
    });

  // Declare ledger before payments-api — the reverse of the order they
  // were added in — and watch the waves swap. This drives the real
  // endpoint: `PUT …/edges` was finished, routed and documented for a
  // long time with nothing in the product able to call it.
  await page.getByRole('button', { name: 'Edit order' }).click();
  await page.getByRole('button', { name: 'Add dependency' }).click();
  await page.getByRole('combobox', { name: 'Dependency 1: lands first' }).click();
  await page.getByRole('option', { name: new RegExp(`^ledger/${CS_MEMBERS.ledger.key}`) }).click();
  await page.getByRole('combobox', { name: 'Dependency 1: lands after' }).click();
  await page
    .getByRole('option', { name: new RegExp(`^payments-api/${CS_MEMBERS['payments-api'].key}`) })
    .click();
  await page.getByRole('button', { name: 'Save order' }).click();
  await expect.poll(async () => page.getByRole('button', { name: 'Edit order' }).count(), {
    timeout: 10000,
  }).toBe(1);

  // The table follows the plan: ledger is now first. Asserting the table
  // rather than the strip is deliberate — the strip could be right while
  // the lander walks a different order, and the table is what the rest of
  // this walkthrough already trusts.
  //
  // Polled, not read once. Waiting for the Edit-order button to come back
  // waits on the *sheet closing*, a proxy for the save; the row order is
  // what the next step actually depends on. Reading it immediately
  // reported "the declared order did not reach the members table" while a
  // screenshot taken moments later showed that it had — a race in the
  // assertion, dressed as a product bug.
  const inLandingOrder = async () => {
    const rows = await page.locator('main table tbody tr').allTextContents();
    const apiAt = rows.findIndex((r) => r.includes(CS_MEMBERS['payments-api'].key));
    const ledgerAt = rows.findIndex((r) => r.includes(CS_MEMBERS.ledger.key));
    return ledgerAt !== -1 && apiAt !== -1 && ledgerAt < apiAt;
  };
  try {
    await expect.poll(inLandingOrder, { timeout: 15000 }).toBe(true);
  } catch {
    const rows = await page.locator('main table tbody tr').allTextContents();
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `the declared order did not reach the members table: ${JSON.stringify(rows)}`,
    });
  }
  await shot(page, '16c2-changeset-order', 'the landing order, as waves, and the edge that set it');

  // Put it back: every later stage in this file lands this changeset and
  // reads both trunks, and a stage that leaves the order reversed would
  // hand them a different product to test.
  await page.getByRole('button', { name: 'Edit order' }).click();
  await page.getByRole('button', { name: /^Remove dependency 1/ }).click();
  await page.getByRole('button', { name: 'Save order' }).click();
  await expect.poll(async () => strip.getByText(/No declared dependencies/).count(), {
    timeout: 10000,
  }).toBe(1);
  console.log('  declared an edge, the waves and the table followed, then restored');
});

await step('changesets / the workspace is one checkout of every member', async () => {
  // The clone URL on the screen is a real repository: one commit on
  // `workspace`, one submodule per member pinned at its proposed head.
  // Clone it with the real git CLI under an ordinary read token and
  // fsck the result — a workspace that only renders is not a workspace.
  const field = page.getByLabel('HTTPS clone URL');
  await field.waitFor({ timeout: 15000 });
  const httpsUrl = await field.inputValue();
  console.log(`  workspace at ${httpsUrl}`);
  const minted = await page.evaluate(async () => {
    const r = await fetch('/v1/orgs/acme/tokens', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ scopes: ['repo:read'], label: 'walkthrough-changeset-workspace' }),
    });
    return { status: r.status, body: await r.json().catch(() => null) };
  });
  if (minted.status !== 201 || !minted.body?.token) {
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `minting a read token for the workspace clone answered ${minted.status}`,
    });
    return;
  }
  csWorkspaceToken = minted.body;
  const remote = new URL(httpsUrl);
  remote.username = 'x';
  remote.password = minted.body.token;
  const dest = path.join(CS_DIR, 'workspace');
  try {
    execFileSync('git', ['clone', '--quiet', '--recurse-submodules', remote.toString(), dest], {
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    execFileSync('git', ['-C', dest, 'fsck', '--full', '--strict'], {
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    for (const [repo, m] of Object.entries(CS_MEMBERS)) {
      const got = fs.readFileSync(path.join(dest, repo, CS_FILE), 'utf8');
      if (got !== m.content)
        problems.push({
          where: stage,
          kind: 'behaviour',
          text: `the workspace's ${repo} submodule does not hold the proposed file: ${JSON.stringify(got)}`,
        });
    }
    const branch = execFileSync('git', ['-C', dest, 'rev-parse', '--abbrev-ref', 'HEAD'], {
      encoding: 'utf8',
    }).trim();
    if (branch !== 'workspace')
      problems.push({ where: stage, kind: 'behaviour', text: `workspace clone is on ${branch}, not workspace` });
    console.log('  cloned with --recurse-submodules; both members at their proposed heads; fsck clean');
  } catch (e) {
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `cloning the workspace failed: ${String(e.stderr ?? e).split('\n')[0]}`,
    });
  }
  await shot(page, '16d-changeset-workspace', 'the whole proposal as one clone command');
});

await step("changesets / the whole set's code reads in one place", async () => {
  // The surface built to hold one review over several repositories has
  // to show the code of all of them: the set's size from the server's
  // own count, one tree per member with the repository on the region,
  // and the open file's header naming its repository before its path.
  // No earlier stage opened this section, so nothing had ever proved the
  // real server's diffstat reached the page or that a real member's
  // tree could be read from it.
  await page.goto(`${BASE}/dashboard/changesets/${CS_KEY}`, { waitUntil: 'networkidle' });
  await page.getByRole('heading', { name: /^Files across 2 repositories$/ }).waitFor({ timeout: 15000 });
  const stat = page.getByText(/\d+ added, \d+ removed/).first();
  const counted = await stat.waitFor({ timeout: 15000 }).then(() => true).catch(() => false);
  if (!counted)
    problems.push({
      where: stage,
      kind: 'content',
      text: 'the set shows no added/removed count — the diffstat route did not reach the page',
    });
  else console.log(`  ${(await stat.textContent())?.trim()}`);

  const group = page.getByRole('group', { name: 'ledger files' });
  const file = CS_FILE.split('/').pop();
  const row = group.getByRole('treeitem', { name: file, exact: true });
  const present = await row.waitFor({ timeout: 15000 }).then(() => true).catch(() => false);
  if (!present) {
    problems.push({ where: stage, kind: 'content', text: `the ledger member's tree has no row for ${file}` });
    return;
  }
  await row.click();
  await page.getByText('tiered fees, booked per tier').first().waitFor({ timeout: 15000 });
  const header = await page.getByText(/^ledger/).first().textContent().catch(() => '');
  if (!/ledger/.test(header ?? ''))
    problems.push({ where: stage, kind: 'content', text: 'the open file does not name its repository' });
  await page.getByLabel(`Viewed ledger/${CS_FILE}`).check();
  await expect(page.getByLabel(`Viewed ledger/${CS_FILE}`)).toBeChecked();
  await shot(page, '16e-changeset-files', "one member's file, read from the set, with its repository named");
  await audit(page, stage);
});

await step('changesets / a member is approved where it is reviewed', async () => {
  // Approval is a property of the change, so it happens on the change:
  // the key in the member row is a link to the change's own page — a
  // forge address, the one a review request carries — and the same
  // review screen the changes stages used. The change knows it is held:
  // the land button the single-change flow pressed is off here, and the
  // page names the changeset it lands with. That name is the way back.
  //
  // The first cut linked the repository name instead, which is the code
  // browser: a page with no approve button on it.
  const approve = async (repo, expectBlocked) => {
    const key = CS_MEMBERS[repo].key;
    await csMemberRow(page, repo).getByRole('link', { name: key }).click();
    await page.waitForURL(`**/acme/${repo}/changes/${key}`, { timeout: 15000 });
    await page.getByText(expectBlocked).waitFor({ timeout: 15000 });
    await approveThroughSheet(page);
    await page.getByText('Landable').waitFor({ timeout: 10000 });
    await page.getByText(`Lands with changeset ${CS_KEY}`).waitFor({ timeout: 10000 });
    const land = page.getByRole('button', { name: 'Land on main' });
    if (await land.isEnabled())
      problems.push({
        where: stage,
        kind: 'behaviour',
        text: `${repo}/${key} is held by a changeset and still offers to land on its own`,
      });
    if (repo === 'payments-api')
      await shot(page, '16e-change-held-by-changeset', 'approved — and held: it lands with its changeset');
    // Back through the link the page offers, across mounts: the forge
    // to the dashboard.
    await page.getByRole('main').getByRole('link', { name: CS_KEY }).click();
    // The forge mount has a changesets route now, so a held change
    // reaches its changeset without leaving the forge — the old
    // dashboard address was a full page load and is no longer where
    // this link goes.
    await page.waitForURL(`**/acme/changesets/${CS_KEY}`, { timeout: 15000 });
    await page.getByRole('heading', { name: CS_TITLE }).waitFor({ timeout: 10000 });
  };
  // The OWNERS-governed member first, then the ungoverned one, which any
  // writer's approval satisfies.
  await approve('payments-api', '■ Blocked');
  await approve('ledger', '■ Blocked');
  console.log('  both members approved on their own pages; each said it lands with the changeset');
});

await step('changesets / land all members', async () => {
  // The previous stage left us on the **forge** changeset page, and the
  // forge deliberately has no org-wide changeset list: its rows are
  // filtered per caller, so a public list would silently hide half of
  // them, and `forgeChangesetLinks` sets `list: null` rather than offer a
  // link to a 404. There is therefore no sidebar and no way back to a
  // list from here — correctly — so the dashboard is reached by address
  // and the row is still the way in from there.
  await page.goto(`${BASE}/dashboard/changesets`);
  // `exact`: accessible-name matching is a substring by default and the
  // detail page carries its own "← All changesets" link.
  await page
    .getByRole('heading', { name: 'Changesets', exact: true })
    .waitFor({ timeout: 10000 });
  await page.getByRole('link', { name: CS_KEY }).click();
  await page.getByRole('heading', { name: CS_TITLE }).waitFor({ timeout: 10000 });
  const verdict = page.locator('section', { hasText: 'Verdict' }).first();
  await verdict.getByText('ready', { exact: true }).waitFor({ timeout: 15000 });
  const land = page.getByRole('button', { name: 'Land all members' });
  await expect(land).toBeEnabled({ timeout: 10000 });
  await shot(page, '16f-changeset-ready', 'every member ready; the set can land');
  await land.click();
  // The view polls the changeset until the queue answers; both trunks
  // move by fast-forward on this stack and it finishes in seconds.
  await page.getByText('Landed: 2 members on their trunks.').waitFor({ timeout: 60000 });
  // Scoped to the changeset's own state badge. The landing-order strip
  // gives every member chip a state too, so a page-wide match now
  // resolves to three elements and trips strict mode — the set's
  // verdict and its members' are different facts and the assertion
  // has to say which one it means.
  await page
    .locator('span[data-slot="badge"]')
    .filter({ hasText: /^● landed$/ })
    .first()
    .waitFor({ timeout: 10000 });
  // A landed set is not a blocked one. The verdict the server answers
  // for it is "not landable: changeset is landed", and the first landed
  // screenshot showed exactly that, read as a gate: the word blocked in
  // red, in the Verdict box and on both rows, over a landing that had
  // gone perfectly. The assertions were green; the picture was wrong.
  await verdict.getByText('landed', { exact: true }).waitFor({ timeout: 10000 });
  if ((await page.getByText('blocked', { exact: true }).count()) !== 0)
    problems.push({
      where: stage,
      kind: 'content',
      text: 'a landed changeset still shows the word "blocked" somewhere on its page',
    });
  await shot(page, '16g-changeset-landed', 'landed in order, each step with the commit it made');
  // Exercise the thing, not the form: both trunks must now serve the
  // file each member added.
  for (const [repo, m] of Object.entries(CS_MEMBERS)) {
    const landed = await page.evaluate(async ({ repo, file }) => {
      const r = await fetch(`/v1/orgs/acme/repos/${repo}/files/${file}?at=main`);
      return { status: r.status, text: await r.text() };
    }, { repo, file: CS_FILE });
    if (landed.status !== 200 || landed.text !== m.content)
      problems.push({
        where: stage,
        kind: 'behaviour',
        text: `${repo}'s trunk does not serve the landed file: ${landed.status} ${JSON.stringify(landed.text)}`,
      });
  }
  console.log(`  both trunks serve ${CS_FILE} after landing`);
});

await step('changesets / revert the set', async () => {
  // Reverting a landed changeset makes a revert change per member on a
  // `revert/<key>` branch and composes them, edges reversed, into a new
  // changeset that lands through the same gate. Nothing moves on any
  // trunk until that one lands — which this run does not do: the
  // reverting set is abandoned below, and both branches are proved to
  // exist rather than trusted from the screen.
  await page.getByRole('button', { name: 'Revert…' }).click();
  const keyField = page.getByLabel('Key for the reverting changeset');
  await keyField.waitFor({ timeout: 5000 });
  const suggested = await keyField.inputValue();
  if (suggested !== `revert-${CS_KEY}`)
    problems.push({
      where: stage,
      kind: 'content',
      text: `the revert key is prefilled as ${JSON.stringify(suggested)}, not revert-${CS_KEY}`,
    });
  await page.getByRole('button', { name: 'Create the revert' }).click();
  await page.waitForURL(`**/dashboard/changesets/revert-${CS_KEY}`, { timeout: 15000 });
  await page.getByText(new RegExp(`^Reverts ${CS_KEY}$`)).waitFor({ timeout: 10000 });
  // Both members, one per repository, on the revert branches.
  await page.locator('tr', { hasText: 'payments-api' }).first().waitFor({ timeout: 10000 });
  const rows = await page.locator('main table tbody tr').allTextContents();
  if (rows.length !== 2 || !rows.some((r) => r.includes('ledger')) || !rows.some((r) => r.includes('payments-api')))
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `the reverting changeset does not have one member per repository: ${JSON.stringify(rows)}`,
    });
  // The branch is named for the *reverting* changeset — `revert/<its
  // key>` — in every member repository.
  const branches = await page.evaluate(async ({ key }) => {
    const out = {};
    for (const repo of ['payments-api', 'ledger']) {
      const r = await fetch(`/v1/orgs/acme/repos/${repo}/branches`);
      out[repo] = JSON.stringify(await r.json().catch(() => null)).includes(`"revert/${key}"`);
    }
    return out;
  }, { key: `revert-${CS_KEY}` });
  for (const [repo, ok] of Object.entries(branches))
    if (!ok)
      problems.push({
        where: stage,
        kind: 'behaviour',
        text: `${repo} has no revert/revert-${CS_KEY} branch after the revert`,
      });
  await shot(page, '16h-changeset-revert', 'the reverting set: one revert per member, edges reversed, nothing landed yet');
  // The audit runs when a stage ends, and this one ends back on the
  // original. Look at *this* page first: every revert member carries a
  // 40-hex Change-Id, the verdict names one, and the first cut let that
  // single token push the Actions column off the right edge.
  await audit(page, stage);
  // The original says who reverted it, in both directions.
  await page.getByRole('link', { name: CS_KEY, exact: true }).click();
  await page.getByText('Reverted by', { exact: false }).waitFor({ timeout: 10000 });
  await page.getByRole('link', { name: `revert-${CS_KEY}` }).click();
  await page.getByText(new RegExp(`^Reverts ${CS_KEY}$`)).waitFor({ timeout: 10000 });
  console.log(`  revert-${CS_KEY} composed; both revert branches exist; nothing landed`);
});

await step('changesets / abandon the revert and leave things as found', async () => {
  // Abandoning the reverting set frees its members: the next run's
  // picker must not be cluttered by this one's. Then the token the
  // workspace clone used goes the way of every credential this
  // walkthrough mints.
  await page.getByRole('button', { name: 'Abandon' }).click();
  await page.getByText(/^⊘ abandoned$/).waitFor({ timeout: 10000 });
  const land = page.getByRole('button', { name: 'Land all members' });
  if (await land.isEnabled())
    problems.push({ where: stage, kind: 'behaviour', text: 'an abandoned changeset offers to land' });
  await shot(page, '16i-changeset-abandoned', 'abandoned: terminal, and it says so');
  // The list, filtered: the landed set and the abandoned one are both
  // there under "all", and only the landed one under "landed".
  // A link, not a button: the way back to a list is a real address,
  // so it is copyable and openable in a new tab. It was a button only
  // while the dashboard was the sole mount that drew it.
  await page.getByRole('link', { name: '← All changesets' }).click();
  await page.getByRole('combobox', { name: 'Filter by state' }).click();
  await page.getByRole('option', { name: 'landed' }).click();
  await page.getByRole('link', { name: CS_KEY, exact: true }).waitFor({ timeout: 10000 });
  if ((await page.getByRole('link', { name: `revert-${CS_KEY}` }).count()) !== 0)
    problems.push({ where: stage, kind: 'behaviour', text: 'the landed filter lists an abandoned changeset' });
  await shot(page, '16j-changesets-list', 'the list, filtered to what landed');
  if (csWorkspaceToken?.id) {
    const revoked = await page.evaluate(async ({ id }) => {
      const r = await fetch(`/v1/orgs/acme/tokens/${encodeURIComponent(id)}`, { method: 'DELETE' });
      return r.status;
    }, { id: csWorkspaceToken.id });
    if (revoked !== 204)
      problems.push({ where: stage, kind: 'behaviour', text: `revoking the workspace token answered ${revoked}` });
  }
  fs.rmSync(CS_DIR, { recursive: true, force: true });
  console.log('  revert abandoned, token revoked, clone removed');
});

// ---------------------------------------------------------------------
// The CI loop, end to end, through a third party.
//
// Everything above this line that involves a check *posts* it — the
// walkthrough is both the CI system and the person watching it, which
// proves the reading side and nothing else. Four seams only exist when
// somebody else is really on the other end of the wire, and none of them
// is covered by any test in this workspace:
//
//   * the outbound webhook fires on a git push at all;
//   * the `X-Weft-Signature-256` we send verifies under a signature
//     check we did not write;
//   * a `repo:read` token clones a private repository from outside;
//   * the intake accepts what a real client sends, and the verdict
//     reaches both the Checks tab and the land gate.
//
// `scripts/manual-stack.sh` stands up the provider —
// `scripts/manual-stack/ci-runner.py`, watching `acme/pipeline` — and
// exports CI_RUNNER_URL and CI_RUNNER_REPO. Without it these stages
// report a missing prerequisite rather than passing quietly, the same
// way a missing SSH URL does: a walkthrough against a half-configured
// stack is a walkthrough of a different product.
// ---------------------------------------------------------------------
const CI_RUNNER = process.env.CI_RUNNER_URL ?? 'http://127.0.0.1:59120';
const CI_REPO = process.env.CI_RUNNER_REPO ?? 'pipeline';
const CI_KEY = `I${(Date.now() + 1).toString(16).padStart(12, '0')}`;
const CI_BRANCH = `ci-review-${Date.now()}`;
const CI_DIR = fs.mkdtempSync(path.join(os.tmpdir(), 'walk-ci-'));
let ciLive = false;
let ciPushToken = null;

// What the provider has done, as it says it. Polled rather than slept
// on: this clones over HTTP and shells out to git, and how long that
// takes is not a constant. Waiting on a timer here is how a stage passes
// against the *previous* verdict.
async function ciRuns() {
  const r = await fetch(`${CI_RUNNER}/runs`);
  return (await r.json()).runs ?? [];
}

async function waitForCi(what, pred, ms = 90000) {
  const deadline = Date.now() + ms;
  let last = [];
  while (Date.now() < deadline) {
    last = await ciRuns().catch(() => []);
    const hit = last.find(pred);
    if (hit) return hit;
    await new Promise((r) => setTimeout(r, 750));
  }
  problems.push({
    where: stage,
    kind: 'behaviour',
    text: `the CI provider never ${what}; its run log was ${JSON.stringify(last).slice(0, 600)}`,
  });
  return null;
}

function ciGit(...args) {
  return execFileSync('git', args, {
    cwd: CI_DIR,
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'pipe'],
  }).trim();
}

await step('ci / a provider is listening', async () => {
  // Ask the provider itself, not the stack script: a process that
  // exited two seconds after `up` printed "stack up" would otherwise be
  // discovered three stages later as a mysterious timeout.
  const alive = await fetch(`${CI_RUNNER}/healthz`)
    .then((r) => r.ok)
    .catch(() => false);
  if (!alive) {
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `no CI provider at ${CI_RUNNER} — the stack is half-configured and the CI stages below prove nothing. Check .stack/logs/ci-runner.log`,
    });
    console.log('  no provider — skipping the CI loop');
    return;
  }
  ciLive = true;
  // A push credential for this run, minted the way a person would and
  // revoked at the end of the loop. Not the org admin token: the point
  // of the next stages is that ordinary credentials do this.
  const minted = await page.evaluate(async ({ repo }) => {
    const r = await fetch('/v1/orgs/acme/tokens', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        scopes: ['repo:read', 'repo:write'],
        repo,
        label: 'walkthrough-ci-push',
      }),
    });
    return { status: r.status, body: await r.json().catch(() => null) };
  }, { repo: CI_REPO });
  if (minted.status !== 201 || !minted.body?.token) {
    ciLive = false;
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `minting a push token for ${CI_REPO} answered ${minted.status}`,
    });
    return;
  }
  ciPushToken = minted.body.token;
  console.log(`  provider up at ${CI_RUNNER}, watching acme/${CI_REPO}`);
});

await step('ci / a push is built by a third party and lands on the Checks tab', async () => {
  if (!ciLive) return;
  // Clone with the token, over HTTP, with the real git CLI — the same
  // thing the provider is about to do with a weaker credential. A repo
  // that cannot be cloned this way has no CI, whatever the tab says.
  const url = new URL(BASE);
  const remote = `${url.protocol}//x:${ciPushToken}@${url.host}/acme/${CI_REPO}.git`;
  execFileSync('git', ['clone', '--quiet', remote, CI_DIR]);
  ciGit('config', 'user.email', 'ada@acme.dev');
  ciGit('config', 'user.name', 'Ada Owner');
  ciGit('checkout', '--quiet', '-b', CI_BRANCH);
  // A commit that the repository's own ci.sh refuses. Red first: a green
  // build proves the pipe carries bytes; a red one proves it carries a
  // *verdict*, and the verdict is the product.
  fs.writeFileSync(path.join(CI_DIR, 'fees.txt'), 'flat 0.1%\nFIXME!! decide on rounding\n');
  ciGit('add', '-A');
  ciGit('commit', '-qm', `adjust the fee schedule\n\nChange-Id: ${CI_KEY}\n`);
  const red = ciGit('rev-parse', 'HEAD');
  ciGit('push', '--quiet', 'origin', CI_BRANCH);
  console.log(`  pushed ${red.slice(0, 8)} to ${CI_BRANCH}`);

  // The whole loop, in one wait: the webhook fired, its signature
  // verified, the clone credential worked, ci.sh ran and said no, and
  // the intake took the verdict.
  //
  // Wait for the REPORT, not for the state. The provider settles
  // `state` before it appends the commit-scoped report, so a predicate
  // that stops at `r.state` returns a run whose `reports` is still empty
  // and the assertion below reads `undefined` off it — which reported
  // itself as "the intake answered undefined to a real signed verdict"
  // and sent the next reader to the intake, where nothing is wrong. Same
  // shape as the form-clear race in CLAUDE.md: wait on the observable
  // the next line actually depends on. The neighbouring land-gate stage
  // already does this; this one was written against the proxy.
  //
  // Proof it was the race and not the intake: the provider's own /runs
  // holds ('commit', 200) for that exact commit when read a moment later.
  const run = await waitForCi(
    'reported on the pushed commit',
    (r) =>
      r.commit === red &&
      r.state &&
      (r.reports ?? []).some((x) => x.scope === 'commit'),
    120000,
  );
  if (!run) return;
  if (run.state !== 'failing')
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `ci.sh refuses this tree, but the provider reported ${run.state}: ${run.summary}`,
    });
  const commitReport = (run.reports ?? []).find((x) => x.scope === 'commit');
  // 200, and only 200. The commit-scoped intake is an upsert — a report
  // under a `(commit, name)` pair nobody has used yet answers the same
  // as one that replaces an earlier verdict — so a client never has to
  // ask whether it created a check or updated one. Only the
  // change-scoped route distinguishes 201 from 200. Accepting 201 here
  // as well would let that contract drift silently, which is exactly how
  // it came to be documented wrongly in the first place.
  if (commitReport?.status !== 200)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `the intake answered ${commitReport?.status} to a real signed verdict, not the documented 200: ${commitReport?.body}`,
    });
  console.log(`  intake answered ${commitReport?.status} to the commit-scoped verdict`);
  console.log(`  provider: ${run.state} — ${run.summary}`);

  // And it is on the tab a person looks at.
  await page.goto(`${BASE}/acme/${CI_REPO}/checks`, { waitUntil: 'networkidle' });
  await page.getByText('ci/local').first().waitFor({ timeout: 15000 });
  const row = page
    .getByRole('listitem')
    .filter({ has: page.locator('time') })
    .filter({ hasText: 'ci/local' })
    .first();
  if (!(await row.count()))
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: 'the verdict reached the intake but no run row is on the Checks tab',
    });
  else if (!(await row.getByRole('img', { name: 'Failing' }).count()))
    problems.push({
      where: stage,
      kind: 'content',
      text: `the Checks tab does not show the run as failing: ${(await row.innerText()).replace(/\s+/g, ' ')}`,
    });
  // The detail link has to go to the provider, not to us: a check whose
  // link comes back here is a check nobody can investigate.
  const href = await row.getByRole('link').first().getAttribute('href').catch(() => null);
  if (!href?.startsWith(CI_RUNNER))
    problems.push({
      where: stage,
      kind: 'content',
      text: `the run's detail link is ${href}, not the provider's own run page`,
    });
  await shot(page, '16b-checks-real', 'a verdict from a real CI provider, on the Checks tab');
});

await step('ci / the land gate holds on the machine, then releases', async () => {
  if (!ciLive) return;
  // Open the review. This is the seam the docs do not mention: a push
  // does not create a patchset, so until somebody does this the
  // provider has a verdict and nowhere to attach it. The provider is
  // sitting in `wait_for_patchset` right now, and this is what ends the
  // wait.
  await page.goto(`${BASE}/dashboard/`, { waitUntil: 'networkidle' });
  await page.getByText('Requests today').waitFor({ timeout: 15000 });
  await page.getByRole('link', { name: CI_REPO, exact: true }).click();
  await page.waitForURL(new RegExp(`/${CI_REPO}$`), { timeout: 15000 });
  await repoTab('Changes').click();
  await page.getByLabel('Branch to review').fill(CI_BRANCH);
  await page.getByRole('button', { name: 'Start review' }).click();
  await page.getByRole('button', { name: /^Review patchset/ }).waitFor({ timeout: 15000 });

  const blocked = await waitForCi(
    'attached a failing verdict to the change',
    (r) =>
      (r.reports ?? []).some(
        (x) => x.scope === 'patchset' && x.change === CI_KEY && x.status,
      ),
    120000,
  );
  if (!blocked) return;
  const attach = blocked.reports.find((x) => x.scope === 'patchset');
  console.log(`  intake answered ${attach.status} to the patchset-scoped verdict`);
  if (attach.status !== 201 && attach.status !== 200)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `a patchset-scoped verdict from a real client answered ${attach.status}: ${attach.body}`,
    });

  // Out to the list and back in, rather than a reload: `Start review`
  // leaves the browser on the change *detail*, whose route has no
  // key-named button on it, and re-entering is also how a person would
  // pick up a verdict that arrived while they were reading.
  await page.getByRole('button', { name: '← Changes' }).click();
  await page.getByRole('button', { name: CI_KEY }).click();
  await page.getByText('ci/local').first().waitFor({ timeout: 15000 });
  await page.getByText(/checks? failed: ci\/local/).waitFor({ timeout: 15000 });
  if (await page.getByRole('button', { name: 'Land on main' }).isEnabled())
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: "the Land button is enabled while the provider's verdict is failing",
    });
  await shot(page, '16c-gate-held', "a third party's red verdict holding the land gate shut");
  console.log('  land gate held on a verdict this run did not produce');

  // Now fix it, for real: the same commit-message trailer, a tree ci.sh
  // accepts, pushed the same way.
  fs.writeFileSync(path.join(CI_DIR, 'fees.txt'), 'flat 0.1%, rounded half up\n');
  ciGit('add', '-A');
  ciGit('commit', '-qm', `round the fee half up\n\nChange-Id: ${CI_KEY}\n`);
  const green = ciGit('rev-parse', 'HEAD');
  ciGit('push', '--quiet', 'origin', CI_BRANCH);
  const built = await waitForCi(
    'built the fixed commit',
    (r) => r.commit === green && r.state,
    120000,
  );
  if (!built) return;
  if (built.state !== 'passing')
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `ci.sh accepts this tree, but the provider reported ${built.state}: ${built.summary}`,
    });

  // The second patchset, the same way as the first — and the provider,
  // waiting again, attaches the green verdict to it.
  await page.getByRole('button', { name: '← Changes' }).click();
  await page.getByLabel('Branch to review').fill(CI_BRANCH);
  await page.getByRole('button', { name: 'Start review' }).click();
  await page.getByRole('button', { name: /^Review patchset/ }).waitFor({ timeout: 20000 });
  const released = await waitForCi(
    'attached a passing verdict to patchset 2',
    (r) =>
      r.commit === green &&
      (r.reports ?? []).some(
        (x) => x.scope === 'patchset' && x.change === CI_KEY && x.status,
      ),
    120000,
  );
  if (!released) return;
  const ok = released.reports.find((x) => x.scope === 'patchset');
  if (ok.status !== 201 && ok.status !== 200)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `greening the change through the intake answered ${ok.status}: ${ok.body}`,
    });
  console.log(`  provider greened ${CI_KEY} at patchset 2 (${ok.status})`);
});

await step('ci / a required check nobody reports is visible before Land', async () => {
  if (!ciLive) return;
  // The gap this stage exists to close. Every other `ci /` stage holds
  // the gate with a check that reported *failing* — the path that always
  // rendered. The path that shipped broken is a required check that has
  // never reported at all: it has no row, so a page counting rows saw
  // one green check, said "All checks have passed", and left Land
  // enabled over a change the queue would hold for its full 30-minute
  // wait budget. Nothing in this walkthrough had ever required a check.
  // Protect main first. Not a detail: `require_check` answers 409 on an
  // unprotected branch — "protect it first, or a required check is one
  // anyone can push past" — and this stage failed exactly that way the
  // first time it ran, which is how the interface it drives learned to
  // stop offering a form that could only ever be refused.
  const put = await page.evaluate(async ({ repo }) => {
    const p = await fetch(`/v1/orgs/acme/repos/${repo}/protections`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ branch: 'main' }),
    });
    if (p.status !== 201 && p.status !== 200)
      return { status: p.status, body: `protecting main: ${await p.text()}` };
    const r = await fetch(`/v1/orgs/acme/repos/${repo}/required-checks/main`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ name: 'ci/never' }),
    });
    return { status: r.status, body: await r.text() };
  }, { repo: CI_REPO });
  if (put.status !== 201 && put.status !== 200) {
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `requiring a check answered ${put.status}: ${put.body}`,
    });
    return;
  }

  // Come back in from the top, the way a person returning to a settings
  // page does — and the way the neighbouring `ci /` stages already do it.
  //
  // The reason is worth keeping: the policy panel mounted when this page
  // first loaded, with main unprotected and nothing required; it re-reads
  // after its *own* actions, and a tab switch is not one, so asserting
  // without a fresh mount tests React's render cache and reports a
  // correct panel as broken.
  //
  // A `goto` is now the whole of it. This used to have to come in from
  // the dashboard's repository list and click a row, because the repo
  // was held in state and had no address to reload — which is the defect
  // this change removed. The settings page is an address; ask for it.
  await page.goto(`${BASE}/acme/${CI_REPO}/settings`, { waitUntil: 'networkidle' });

  // A maintainer must be able to see what they just required without
  // curl — the whole surface had a REST API and no interface at all.
  //
  // Branch policy is under Settings. It used to be on the dashboard's
  // own repo screen, and this comment used to say so and warn against
  // reaching for "the forge's furniture" — there is one repository page
  // now, and its furniture is the only furniture.
  await page.getByText('Branch policy').waitFor({ timeout: 15000 });
  // Wait for the row, do not count for it. The requirements are a second
  // fetch made after the panel paints, so "Branch policy is on screen"
  // is a proxy for "the list has arrived" and not the thing itself — a
  // bare `count()` here reads the Loading… state and reports a panel
  // that renders perfectly well as broken.
  const listed = await page
    .getByText('ci/never', { exact: true })
    .first()
    .waitFor({ timeout: 15000 })
    .then(() => true)
    .catch(() => false);
  if (!listed)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: 'Branch policy does not list the check just required on main',
    });

  // And the change page must say it, in both places a reader looks.
  await repoTab('Changes').click();
  await page.getByRole('button', { name: CI_KEY }).click();
  await page
    .getByText(/1 required check has not reported: ci\/never/)
    .waitFor({ timeout: 15000 });
  if (await page.getByText('All checks have passed').count())
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: 'the checks panel claims a pass while a required check has never reported',
    });
  await shot(
    page,
    '16e-required-missing',
    'a required check nobody has reported, named before anybody presses Land',
  );

  // Put the branch back as this run found it, and prove the page follows
  // — a stale blocker is as misleading as a missing one.
  const del = await page.evaluate(async ({ repo }) => {
    const r = await fetch(
      `/v1/orgs/acme/repos/${repo}/required-checks/main?name=${encodeURIComponent('ci/never')}`,
      { method: 'DELETE' },
    );
    // Unprotect too: the stage that follows lands this change, and it
    // must land the way every other run of this walkthrough lands it.
    await fetch(`/v1/orgs/acme/repos/${repo}/protections/main`, {
      method: 'DELETE',
    });
    return r.status;
  }, { repo: CI_REPO });
  if (del !== 200 && del !== 204)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `removing the requirement answered ${del}`,
    });
  await page.getByRole('button', { name: '\u2190 Changes' }).click();
  await page.getByRole('button', { name: CI_KEY }).click();
  await page.getByText('All checks have passed').waitFor({ timeout: 15000 });
  console.log('  a never-reported requirement was named, then cleared');
});

await step('ci / approve and land on the provider\'s word', async () => {
  if (!ciLive) return;
  await page.getByRole('button', { name: '← Changes' }).click();
  await page.getByRole('button', { name: CI_KEY }).click();
  await page.getByText('Passing', { exact: true }).first().waitFor({ timeout: 20000 });
  await approveThroughSheet(page);
  await page.getByText('Landable').waitFor({ timeout: 15000 });
  await page.getByRole('button', { name: 'Land on main' }).click();
  await page.getByText('Landed as').waitFor({ timeout: 30000 });
  await shot(page, '16d-gate-released', 'green from a real provider, approved, landed');
  // Exercise the thing, not the form: trunk serves what the change
  // carried, and it is the version ci.sh accepted.
  const landed = await page.evaluate(async ({ repo }) => {
    const r = await fetch(`/v1/orgs/acme/repos/${repo}/files/fees.txt?at=main`);
    return { status: r.status, text: await r.text() };
  }, { repo: CI_REPO });
  if (landed.status !== 200 || !landed.text.includes('rounded half up'))
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `trunk does not serve the landed file: ${landed.status} ${landed.text.slice(0, 80)}`,
    });

  // Leave the org as this run found it: the push credential goes.
  const gone = await page.evaluate(async () => {
    const list = await fetch('/v1/orgs/acme/tokens').then((r) => r.json());
    // The one THIS run minted, not the first row that shares its label.
    // A revoked token stays listed, so a second pass over one stack
    // finds the previous pass's dead token, deletes it, and reports the
    // 404 as the product refusing to revoke a live credential.
    const mine = (list.tokens ?? [])
      .filter((t) => t.label === 'walkthrough-ci-push' && !t.revoked_at)
      .pop();
    if (!mine) return 'no such token in the list';
    const r = await fetch(`/v1/orgs/acme/tokens/${mine.id}`, { method: 'DELETE' });
    return r.status;
  });
  if (gone !== 204)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `revoking the walkthrough's push token answered ${JSON.stringify(gone)}`,
    });
  fs.rmSync(CI_DIR, { recursive: true, force: true });
  console.log('  landed on a verdict produced outside this process; credential revoked');
});

// ---------------------------------------------------------------------
// Hosted runners: the workflows the forge runs ITSELF.
//
// The `ci /` stages above prove the loop when somebody else owns the
// build. These prove the other one: a workflow file in the repository,
// a job dispatched onto a runner we started, and a verdict that arrives
// on the same Checks tab without anybody posting it.
//
// Nothing here is mocked in the browser. The push is the real git CLI
// over HTTP with a minted token; the dispatch is the app signing a real
// SigV4 RunTask; `deploy/fake-ecs` answers it by starting the REAL
// runner image in a container, which clones back over HTTP with its job
// token and reports its own verdict. The only thing standing in for
// production is the cloud.
//
// `scripts/manual-stack.sh` brings that stand-in up and exports
// RUNNER_ECS_URL and RUNNER_WF_REPO. Without it these stages report a
// missing prerequisite rather than passing quietly — the same rule as a
// missing SSH URL and the same rule as the CI provider: a walkthrough
// against a half-configured stack is a walkthrough of a different
// product.
// ---------------------------------------------------------------------
const WF_ECS = process.env.RUNNER_ECS_URL ?? 'http://127.0.0.1:59130';
const WF_REPO = process.env.RUNNER_WF_REPO ?? 'builds';
const WF_KEY = `I${(Date.now() + 2).toString(16).padStart(12, '0')}`;
const WF_BRANCH = `wf-review-${Date.now()}`;
const WF_DIR = fs.mkdtempSync(path.join(os.tmpdir(), 'walk-wf-'));
let wfLive = false;
let wfPushToken = null;
let forkSha = null;

// A workflow whose steps say something about the environment they ran
// in, so a green run means the runner really checked the commit out and
// really had the job's variables — not merely that a shell exited 0.
const WF_FAST = `name: ci
on: [push, change]
jobs:
  test:
    steps:
      - name: Sanity
        run: echo "job=$WEFT_JOB sha=$WEFT_SHA event=$WEFT_EVENT"
      - name: Files
        run: test -f README.md
`;

// Long enough that a second push lands while it is still running. This
// is the only way to observe superseding: a job that finishes first has
// nothing left to cancel.
//
// It counts out loud rather than sleeping in one lump, because the
// superseding stage asserts on where the log STOPPED. A single `sleep 40`
// produces the same log whether the container was stopped on time or ran
// to the end; ticks say which.
const WF_TICKS = 8;
const WF_SLOW = `name: ci
on: [push, change]
jobs:
  test:
    steps:
      - name: Tick
        run: for i in $(seq 1 ${WF_TICKS}); do echo "tick $i"; sleep 2; done
`;

// git's own stderr, in the error. `execFileSync` throws with only the
// command line in its message, so a rejected push reported itself as
// "Command failed: git push --quiet origin <branch>" and said nothing
// about why — which cost a debugging round trip on the very first run of
// these stages, against a server that was answering with a perfectly
// clear reason.
function wfGit(...args) {
  try {
    return execFileSync('git', args, {
      cwd: WF_DIR,
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe'],
    }).trim();
  } catch (e) {
    const said = `${e.stderr ?? ''}${e.stdout ?? ''}`.trim();
    throw new Error(`git ${args.join(' ')} failed: ${said || e.message}`);
  }
}

let wfStamp = 0;

function wfCommit(message, workflow, extra) {
  fs.mkdirSync(path.join(WF_DIR, '.weft'), { recursive: true });
  // The workflow file is written verbatim, so a commit that goes back to
  // a workflow an earlier commit already pushed re-sends a blob the
  // server holds. The superseding stage depends on that; see the note
  // there.
  fs.writeFileSync(path.join(WF_DIR, '.weft/ci.yml'), workflow);
  // A file that is different every time, so every one of these commits
  // has something in it.
  //
  // Without it the stage depends on what the repository happened to
  // contain already: a stack that has been pushed to — by a previous
  // run, or by somebody checking something by hand — can already carry
  // this exact workflow, `git commit` then finds nothing staged and
  // fails, and the failure reads as the push path being broken. The
  // first run of this stage failed exactly that way against a repository
  // whose main already had the same file.
  wfStamp += 1;
  fs.writeFileSync(
    path.join(WF_DIR, 'walkthrough.txt'),
    `${WF_BRANCH} step ${wfStamp}\n`,
  );
  wfGit('add', '-A');
  wfGit('commit', '-qm', extra ? `${message}\n\n${extra}\n` : message);
  return wfGit('rev-parse', 'HEAD');
}

// The runs this repository has, as the product reports them. Read through
// the page so it goes out under the signed-in session, the same way the
// interface reads it.
async function wfRuns() {
  return page.evaluate(async (repo) => {
    const r = await fetch(`/v1/orgs/acme/repos/${repo}/workflow-runs?limit=20`);
    if (!r.ok) return [];
    return (await r.json()).runs ?? [];
  }, WF_REPO);
}

// Polled, never slept on. How long a container takes to pull, boot, clone
// and build is not a constant, and waiting on a timer here is how a stage
// passes against the previous run.
async function waitForRun(what, pred, ms = 120000) {
  const deadline = Date.now() + ms;
  let last = [];
  while (Date.now() < deadline) {
    last = await wfRuns().catch(() => []);
    const hit = last.find(pred);
    if (hit) return hit;
    await new Promise((r) => setTimeout(r, 750));
  }
  problems.push({
    where: stage,
    kind: 'behaviour',
    text: `no run ever ${what}; the run list was ${JSON.stringify(
      last.map((r) => ({ sha: r.commit_sha?.slice(0, 8), ref: r.ref_name, state: r.state, error: r.error })),
    ).slice(0, 600)}`,
  });
  return null;
}

const WF_STATES = ['Queued', 'Running', 'Passing', 'Failing', 'Cancelled', 'Skipped', 'Blocked'];

// One check row, by name, from either of the two places the product
// draws one — and they are not the same markup, which is the whole
// reason this takes an argument.
//
// The Checks tab draws a run per list item with a timestamp and a
// **labelled** state glyph, so the state is readable as an image role.
// The strip on a commit page has no timestamp and its glyph is
// `aria-hidden`, with the state printed beside it as a word. A helper
// written against the first shape finds nothing at all on the second:
// the first run of these stages reported "no check row at all" on two
// commit pages that were rendering their rows perfectly well.
// What the ECS stand-in has running, in DescribeTasks' shape.
//
// Node-side, because this is not the product's origin and the browser
// would refuse to read it. `/tasks` is the stand-in's own read-only
// window (`deploy/fake-ecs/fake-ecs.py`), added so a stage can ask
// whether the compute really stopped without holding the docker socket.
async function wfTasks() {
  return fetch(`${WF_ECS}/tasks`)
    .then((r) => (r.ok ? r.json() : { tasks: [] }))
    .then((b) => b.tasks ?? [])
    .catch(() => []);
}

// Follow a check row's own link to the run behind it.
//
// The row is the only handle a reader has, so this clicks what they
// would click rather than navigating to a URL the test made up. A link
// that opens in a new tab is followed into that tab and handed back, so
// the stage works whichever way the row is rendered.
async function wfFollowRunLink(runId) {
  const link = page.locator(`a[href*="/checks/runs/${runId}"]`).first();
  if (!(await link.count())) return null;
  const href = await link.getAttribute('href');
  const text = (await link.innerText()).replace(/\s+/g, ' ').trim();
  const popup = page
    .context()
    .waitForEvent('page', { timeout: 4000 })
    .catch(() => null);
  await link.click();
  const opened = await popup;
  const on = opened ?? page;
  await on.waitForLoadState('networkidle');
  return { href, text, page: on, popup: opened };
}

async function wfCheckRow(name, { strip = false } = {}) {
  const named = page.getByRole('listitem').filter({ hasText: name });
  const row = (strip ? named : named.filter({ has: page.locator('time') })).first();
  if (!(await row.count())) return null;
  const text = (await row.innerText()).replace(/\s+/g, ' ');
  for (const st of WF_STATES) {
    if (await row.getByRole('img', { name: st }).count()) return { state: st, text };
  }
  return { state: WF_STATES.find((st) => new RegExp(`\\b${st}\\b`).test(text)) ?? null, text };
}

// Whether a check row is telling the reader how long something took.
//
// A run that never got a machine — a refused file, a held fork, a budget
// that is spent — has no duration, and the row must not invent one. The
// first version of the mirror stamped the run's *creation* as its start,
// and the tab then printed "0s" beside a refused file (an instant pass,
// to the eye) and "3m 47s so far", climbing forever, beside a run that
// was waiting on a person. "40s ago" is the row's age, not a duration,
// and is left alone.
function wfRowShowsDuration(text) {
  return /\bso far\b/.test(text) || /\b\d+(?:s|m \d+s|h \d+m)\b(?! ago)/.test(text);
}

function wfExpectNoDuration(row, what) {
  if (row && wfRowShowsDuration(row.text))
    problems.push({
      where: stage,
      kind: 'content',
      text: `${what} never ran, but its check row shows a duration: ${row.text}`,
    });
}

await step('workflows / a hosted runner is configured', async () => {
  // Ask the stand-in itself rather than trusting the stack script: a
  // process that died two seconds after `up` printed its banner would
  // otherwise be discovered four stages later as a mysterious timeout.
  const alive = await fetch(`${WF_ECS}/`)
    .then((r) => r.ok)
    .catch(() => false);
  if (!alive) {
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `no runner dispatch endpoint at ${WF_ECS} — this stack cannot run a workflow of its own, so the workflow stages below prove nothing. Bring it up with scripts/manual-stack.sh (it needs a docker daemon and the weft-runner:local image) and check .stack/logs/fake-ecs.log`,
    });
    console.log('  no hosted runner — skipping the workflow loop');
    return;
  }
  // Back to a signed-in page: the CI stages above finish on a change
  // detail, and everything below reads the API through this session.
  await page.goto(`${BASE}/dashboard/`, { waitUntil: 'networkidle' });
  await page.getByText('Requests today').waitFor({ timeout: 15000 });
  const minted = await page.evaluate(async (repo) => {
    const r = await fetch('/v1/orgs/acme/tokens', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        scopes: ['repo:read', 'repo:write'],
        repo,
        label: 'walkthrough-workflow-push',
      }),
    });
    return { status: r.status, body: await r.json().catch(() => null) };
  }, WF_REPO);
  if (minted.status !== 201 || !minted.body?.token) {
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `minting a push token for ${WF_REPO} answered ${minted.status} — is the repo seeded? scripts/manual-stack.sh creates acme/${WF_REPO}`,
    });
    return;
  }
  // A stack this walkthrough already ran against is suspended — the
  // runtime-miner stage below does that on purpose — and every workflow
  // stage would then report blocked runs. Say which it is up front.
  const bill = await page.evaluate(async () => (await fetch('/v1/orgs/acme/billing')).json());
  if (bill.ci_suspended_reason) {
    problems.push({
      where: stage,
      kind: 'harness',
      text: `acme's hosted workflows are already suspended (${JSON.stringify(bill.ci_suspended_reason)}) — this is a stack a previous run left behind; bring it up fresh (scripts/manual-stack.sh down && up)`,
    });
    return;
  }
  wfPushToken = minted.body.token;
  wfLive = true;
  console.log(`  runner dispatch up at ${WF_ECS}, building acme/${WF_REPO}`);
});

await step('workflows / a push runs the repository\'s own workflow', async () => {
  if (!wfLive) return;
  const url = new URL(BASE);
  const remote = `${url.protocol}//x:${wfPushToken}@${url.host}/acme/${WF_REPO}.git`;
  execFileSync('git', ['clone', '--quiet', remote, WF_DIR]);
  wfGit('config', 'user.email', 'ada@acme.dev');
  wfGit('config', 'user.name', 'Ada Owner');
  const sha = wfCommit('build on every push', WF_FAST);
  wfGit('push', '--quiet', 'origin', 'main');
  console.log(`  pushed ${sha.slice(0, 8)} to main`);

  // The whole loop in one wait: the push trigger fired, the file parsed,
  // a job was planned, RunTask was signed and accepted, a container
  // booted, cloned back with its job token, ran both steps and reported.
  const run = await waitForRun(
    'reported on the pushed commit',
    (r) => r.commit_sha === sha && r.state !== 'running',
  );
  if (!run) return;
  if (run.state !== 'passed')
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `this workflow should pass, but the run is ${run.state}: ${run.error ?? ''} — jobs ${JSON.stringify(
        (run.jobs ?? []).map((j) => [j.key, j.state, j.error]),
      )}`,
    });
  if (run.name !== 'ci')
    problems.push({
      where: stage,
      kind: 'content',
      text: `the run is named ${JSON.stringify(run.name)}, not the workflow's own \`name: ci\``,
    });

  // And a person sees it, on the same tab that carries third parties'
  // verdicts — no separate place to look, which is the point of
  // mirroring a job into a check run.
  await page.goto(`${BASE}/acme/${WF_REPO}/checks`, { waitUntil: 'networkidle' });
  await page.getByText('ci / test').first().waitFor({ timeout: 20000 });
  const row = await wfCheckRow('ci / test');
  if (!row)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: 'the run passed but no row for it is on the Checks tab',
    });
  else if (row.state !== 'Passing')
    problems.push({
      where: stage,
      kind: 'content',
      text: `the Checks tab shows the hosted run as ${row.state ?? 'no state at all'}: ${row.text}`,
    });
  await shot(page, '16f-workflow-passing', "the forge's own runner, on the Checks tab");
  console.log(`  hosted run ${run.id} passed and is on the Checks tab`);
});

await step('workflows / the same workflow runs for a change', async () => {
  if (!wfLive) return;
  wfGit('checkout', '--quiet', '-b', WF_BRANCH);
  fs.writeFileSync(path.join(WF_DIR, 'notes.md'), 'a first note\n');
  const sha = wfCommit('add a note', WF_FAST, `Change-Id: ${WF_KEY}`);
  wfGit('push', '--quiet', 'origin', WF_BRANCH);

  // Opening the review is what creates the patchset — a push alone does
  // not — and `on: [push, change]` means this starts a SECOND run for
  // the same commit, under the change event. Both are the product.
  await page.goto(`${BASE}/dashboard/`, { waitUntil: 'networkidle' });
  await page.getByText('Requests today').waitFor({ timeout: 15000 });
  await page.getByRole('link', { name: WF_REPO, exact: true }).click();
  await page.waitForURL(new RegExp(`/${WF_REPO}$`), { timeout: 15000 });
  await repoTab('Changes').click();
  await page.getByLabel('Branch to review').fill(WF_BRANCH);
  await page.getByRole('button', { name: 'Start review' }).click();
  await page.getByRole('button', { name: /^Review patchset/ }).waitFor({ timeout: 20000 });

  const run = await waitForRun(
    'built the change',
    (r) => r.commit_sha === sha && r.event === 'change' && r.state !== 'running',
  );
  if (!run) return;
  if (run.state !== 'passed')
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `the change's run is ${run.state}: ${run.error ?? ''}`,
    });
  if (run.change_key !== WF_KEY)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `the change run carries change_key ${JSON.stringify(run.change_key)}, not ${WF_KEY}`,
    });

  // On the change itself, where the decision to land is made.
  await page.getByRole('button', { name: '← Changes' }).click();
  await page.getByRole('button', { name: WF_KEY }).click();
  await page.getByText('ci / test').first().waitFor({ timeout: 20000 });
  await page.getByText('All checks have passed').waitFor({ timeout: 20000 });
  await shot(page, '16g-workflow-change', "a hosted run standing beside a change");
  console.log(`  the change's own build passed at patchset 1`);
});

await step('workflows / a newer push cancels the run it replaced', async () => {
  if (!wfLive) return;
  // A slow job, so there is something still running to cancel. Without
  // this the second push arrives after the first has already settled and
  // there is nothing to observe — the stage would pass while proving
  // that superseding never happened.
  fs.writeFileSync(path.join(WF_DIR, 'notes.md'), 'a second note\n');
  const stale = wfCommit('take a while', WF_SLOW, `Change-Id: ${WF_KEY}`);
  wfGit('push', '--quiet', 'origin', WF_BRANCH);
  const running = await waitForRun(
    'started building the slow commit',
    (r) => r.commit_sha === stale && r.event === 'push' && r.state === 'running',
    90000,
  );
  if (!running) return;
  console.log(`  ${stale.slice(0, 8)} is building`);

  // Wait until it is really executing the step, not merely dispatched.
  // A run turns `running` when the job is claimed, which is before the
  // container has booted and cloned — supersede it then and there is
  // nothing to stop, no log to end early, and the assertions below would
  // report an empty log as a defect of the product rather than of their
  // own timing. The first log chunk is the observable that says the
  // step is underway.
  const ticking = await waitForRun(
    'the superseded job started printing',
    (r) => r.id === running.id && (r.jobs ?? []).some((j) => (j.log_chunks ?? 0) > 0),
    60000,
  );
  if (!ticking) return;

  // Now replace it while it is in flight — with a commit that goes
  // BACKWARDS: `notes.md` returns to the exact bytes of the commit that
  // opened this branch, and the workflow returns to the fast one main
  // already carries. Both blobs, and the `.weft` tree holding one of
  // them, are objects the server already has, and git re-sends them
  // because a thin pack only marks the boundary commit's trees
  // uninteresting — an older ancestor's blobs are not excluded.
  //
  // This push used to be refused outright, with `object <oid> already
  // present (concurrent push?) — fetch and retry`, which no fetch could
  // ever clear. Reverting a file, `git revert` and restoring a deleted
  // file all have this shape. Keep it here: the stage is the
  // walkthrough's regression check for that, on top of proving that a
  // newer push cancels the run it replaced.
  fs.writeFileSync(path.join(WF_DIR, 'notes.md'), 'a first note\n');
  const fresh = wfCommit('be quick about it', WF_FAST, `Change-Id: ${WF_KEY}`);
  wfGit('push', '--quiet', 'origin', WF_BRANCH);

  const killed = await waitForRun(
    'cancelled the superseded run',
    (r) => r.commit_sha === stale && r.event === 'push' && r.state === 'cancelled',
    90000,
  );
  if (killed && !/superseded/.test(killed.error ?? ''))
    problems.push({
      where: stage,
      kind: 'content',
      text: `the superseded run was cancelled but says ${JSON.stringify(killed.error)}, which does not tell a reader why`,
    });

  // The row says cancelled — but does the COMPUTE stop? This is the
  // whole point of superseding, and it is invisible from the product's
  // own API: the run is marked cancelled the moment StopTask is
  // accepted, so every assertion above passes just as happily against a
  // container that carries on building for another minute. Ask the thing
  // that is actually running it.
  //
  // A stack whose StopTask fired on time and whose container ignored it
  // ran every remaining step and then failed trying to report a verdict
  // nobody wanted — which is worse than not cancelling at all, because
  // it looks cancelled.
  const staleRun = (await wfRuns()).find((r) => r.id === (killed ?? running).id) ?? running;
  const staleJobs = new Set(
    (staleRun.jobs ?? []).flatMap((j) => [j.id, j.job_id]).filter(Boolean),
  );
  const mine = (t) => [...staleJobs].some((id) => (t.startedBy ?? '').includes(id));
  let alive = [];
  const stopBy = Date.now() + 6000;
  do {
    alive = (await wfTasks()).filter((t) => mine(t) && t.lastStatus === 'RUNNING');
    if (!alive.length) break;
    await page.waitForTimeout(500);
  } while (Date.now() < stopBy);
  if (alive.length)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `the run reads Cancelled but its container is still RUNNING six seconds later (${alive
        .map((t) => t.taskArn.split('/').pop().slice(0, 12))
        .join(', ')}) — the job was only cancelled on paper and is still burning a task`,
    });
  else console.log('  the superseded task really stopped');

  const winner = await waitForRun(
    'built the replacing commit',
    (r) => r.commit_sha === fresh && r.event === 'push' && r.state !== 'running',
  );
  if (winner && winner.state !== 'passed')
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `the replacing commit's run is ${winner.state}: ${winner.error ?? ''}`,
    });

  // And a person reading the superseded commit is told it was cancelled,
  // rather than left looking at a run that appears to still be going.
  await page.goto(`${BASE}/acme/${WF_REPO}/commit/${stale}`, { waitUntil: 'networkidle' });
  await page.getByText('ci / test').first().waitFor({ timeout: 20000 });
  const supersededRow = await wfCheckRow('ci / test', { strip: true });
  if (!supersededRow)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `the superseded commit ${stale.slice(0, 8)} shows no check row at all`,
    });
  else if (supersededRow.state !== 'Cancelled')
    problems.push({
      where: stage,
      kind: 'content',
      text: `the superseded commit's check reads ${supersededRow.state ?? 'no state'}, not Cancelled: ${supersededRow.text}`,
    });
  await shot(page, '16h-workflow-superseded', 'the run a newer push replaced, reading Cancelled');

  // And on the run's own page: why it stopped, and where it got to.
  //
  // The log is the second half of the same question the task check asks
  // — a container that was stopped at the fourth tick cannot have
  // printed the eighth, and a run whose log reaches the end was never
  // cancelled in any sense that matters.
  await page.goto(`${BASE}/acme/${WF_REPO}/checks/runs/${staleRun.id}`, {
    waitUntil: 'networkidle',
  });
  const said = await page
    .getByText(`superseded by ${fresh.slice(0, 12)}`)
    .first()
    .waitFor({ timeout: 20000 })
    .then(() => true)
    .catch(() => false);
  if (!said)
    problems.push({
      where: stage,
      kind: 'content',
      text: `the run page does not say "superseded by ${fresh.slice(0, 12)}" — a reader is shown a cancelled run and not told what replaced it`,
    });
  const started = await page
    .getByText('tick 1')
    .first()
    .waitFor({ timeout: 20000 })
    .then(() => true)
    .catch(() => false);
  if (!started)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: 'the superseded run kept no log at all, so there is no way to see how far it got before it was stopped',
    });
  else {
    const log = (await page.innerText('body')).replace(/\s+/g, ' ');
    if (log.includes(`tick ${WF_TICKS}`))
      problems.push({
        where: stage,
        kind: 'behaviour',
        text: `the superseded run's log reaches "tick ${WF_TICKS}", the last step of a workflow that was cancelled ${WF_TICKS * 2}s of work earlier — the container ran to completion and the cancellation was on paper only`,
      });
  }
  await shot(page, '16h2-workflow-superseded-run', 'a superseded run: why it stopped, and where its log ends');
  console.log('  the superseded run was cancelled and says so');
});

await step('workflows / a check row leads to the run, and the run shows its log', async () => {
  if (!wfLive) return;
  // A check that says "failing" and goes nowhere is a dead end: the next
  // thing a person wants is the output of the step that failed. This
  // walks the way they get there — from the row on the Checks tab, by
  // its own link — rather than reading the log endpoint directly, which
  // would pass even if nothing on the page pointed at it.
  const target = (await wfRuns()).find(
    (r) => r.state === 'passed' && (r.jobs ?? []).length > 0,
  );
  if (!target) {
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: 'no passed hosted run to open — the stages above should have left one',
    });
    return;
  }

  await page.goto(`${BASE}/acme/${WF_REPO}/checks`, { waitUntil: 'networkidle' });
  await page.getByText('ci / test').first().waitFor({ timeout: 20000 });
  const followed = await wfFollowRunLink(target.id);
  if (!followed) {
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `the Checks tab has no link to run ${target.id} — a hosted run's row is a dead end and its log cannot be reached from the interface`,
    });
    return;
  }
  const want = `/acme/${WF_REPO}/checks/runs/${target.id}`;
  if (!followed.href.endsWith(want))
    problems.push({
      where: stage,
      kind: 'content',
      text: `the row links to ${JSON.stringify(followed.href)}, which does not end with ${JSON.stringify(want)}`,
    });

  // The run page itself: the job, and the output of a step in it. The
  // log arrives as a tail, so wait for the text rather than reading once.
  const on = followed.page;
  try {
    await on.getByText('test', { exact: false }).first().waitFor({ timeout: 20000 });
    await on.getByText(/job=test\b/).first().waitFor({ timeout: 40000 });
  } catch {
    const seen = (await on.innerText('body')).replace(/\s+/g, ' ').slice(0, 600);
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `the run page for ${target.id} never showed the step's own output (\`job=test …\`, echoed by the workflow's first step); it showed: ${seen}`,
    });
  }
  await shot(on, '16j-workflow-run-page', "a hosted run's own page, with the log of the step that ran");
  if (followed.popup) await followed.popup.close();
  console.log(`  the row for ${target.id} leads to the run and its log`);
});

await step('workflows / a workflow that does not parse is refused, and says why', async () => {
  if (!wfLive) return;
  // The refusal path has no job, no runner and no container: the file is
  // read, refused, and settled as a failed run before anything is
  // dispatched. It is worth a stage of its own because it is the case a
  // person hits most — a workflow is typed by hand — and because a
  // refusal that reaches nobody is the same as a crash.
  const branch = `wf-broken-${Date.now()}`;
  wfGit('checkout', '--quiet', '-b', branch);
  const sha = wfCommit(
    'a key we do not support',
    'name: ci\non: [push]\npermissions:\n  contents: read\njobs:\n  test:\n    steps:\n      - name: Go\n        run: echo hi\n',
  );
  wfGit('push', '--quiet', 'origin', branch);

  const run = await waitForRun(
    'settled the unparseable workflow',
    (r) => r.commit_sha === sha && r.state !== 'running',
    90000,
  );
  if (!run) return;
  if (run.state !== 'failed')
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `a workflow with an unsupported key settled as ${run.state}, not failed`,
    });
  if ((run.jobs ?? []).length !== 0)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `a refused workflow planned ${run.jobs.length} job(s) — nothing should have been dispatched`,
    });
  // The message is the product here. A refusal that says "invalid" sends
  // the reader to the docs; one that names the key, the line and the
  // alternatives sends them to the fix.
  const why = run.error ?? '';
  for (const want of ['permissions', ':3', 'name, on, jobs']) {
    if (!why.includes(want))
      problems.push({
        where: stage,
        kind: 'content',
        text: `the refusal does not mention ${JSON.stringify(want)}: ${JSON.stringify(why)}`,
      });
  }
  console.log(`  refused: ${why}`);

  // And it reaches the tab, named after the file that could not be read.
  await page.goto(`${BASE}/acme/${WF_REPO}/commit/${sha}`, { waitUntil: 'networkidle' });
  await page.getByText('.weft/ci.yml').first().waitFor({ timeout: 20000 });
  const row = await wfCheckRow('.weft/ci.yml', { strip: true });
  if (!row)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: 'a refused workflow leaves no check on the commit — the push looks like it built nothing at all',
    });
  else if (row.state !== 'Failing')
    problems.push({
      where: stage,
      kind: 'content',
      text: `the refused workflow's check reads ${row.state ?? 'no state'}, not Failing: ${row.text}`,
    });
  wfExpectNoDuration(row, 'the refused workflow');
  await shot(page, '16i-workflow-refused', 'a workflow file that could not be read, refused before anything ran');

  // And the row is a way in, not just a verdict: the reason lives on the
  // run, and the row's link is how a reader gets to it. A refusal that
  // reaches nobody is the same as a crash, and a refusal a reader cannot
  // reach from the failing check is very nearly the same thing.
  //
  // The Checks tab first, and the commit the refusal is on if the tab
  // does not carry a branch that was never merged — the point is that
  // the row a reader is looking at links onwards, not which of the two
  // pages they happened to open.
  await page.goto(`${BASE}/acme/${WF_REPO}/checks`, { waitUntil: 'networkidle' });
  const onTab = await page
    .getByText('.weft/ci.yml')
    .first()
    .waitFor({ timeout: 20000 })
    .then(() => true)
    .catch(() => false);
  if (!onTab)
    await page.goto(`${BASE}/acme/${WF_REPO}/commit/${sha}`, { waitUntil: 'networkidle' });
  const toRun = await wfFollowRunLink(run.id);
  if (!toRun)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `the refused workflow's check row has no link to run ${run.id}, so a reader is told a workflow failed with no way to reach why`,
    });
  else {
    const shown = (await toRun.page.innerText('body')).replace(/\s+/g, ' ');
    if (!shown.includes(why.replace(/\s+/g, ' ')))
      problems.push({
        where: stage,
        kind: 'content',
        text: `the run page does not carry the refusal as the API gives it (${JSON.stringify(why)}); it shows: ${shown.slice(0, 600)}`,
      });
    await shot(toRun.page, '16k-workflow-refused-run', 'the run page for a refused workflow, carrying the reason');
    if (toRun.popup) await toRun.popup.close();
  }

});

// ---------------------------------------------------------------------
// Abuse controls. Four layers refuse a hosted runner being used as a
// free machine: the Network Firewall (not observable from here), the
// file being refused at push time, a running step being killed, and the
// organisation being suspended when that happens. The last three each
// get a stage, and the last one is deliberately the final workflow stage
// in this file: once `acme` is suspended, nothing else it pushes will
// run until an operator clears it, which is the product working — and
// is also why this walkthrough needs a FRESH stack every time.
// ---------------------------------------------------------------------

await step('workflows / a workflow that names a miner is refused before it runs', async () => {
  if (!wfLive) return;
  // The naive case — the program named outright, a pool address beside
  // it. Layer 2 answers the author on the line it is on; nothing is
  // scheduled and nothing is billed.
  const branch = `wf-miner-${Date.now()}`;
  wfGit('checkout', '--quiet', WF_BRANCH);
  wfGit('checkout', '--quiet', '-b', branch);
  const sha = wfCommit(
    'a workflow that mines',
    'name: ci\non: [push]\njobs:\n  build:\n    steps:\n      - name: Build\n        run: |\n          echo building\n          ./xmrig -o stratum+tcp://pool.example:3333\n',
  );
  wfGit('push', '--quiet', 'origin', branch);

  const run = await waitForRun(
    'refused the mining workflow',
    (r) => r.commit_sha === sha && r.state !== 'running',
    90000,
  );
  if (!run) return;
  if (run.state !== 'failed')
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `a workflow that runs a miner settled as ${run.state}, not failed`,
    });
  if ((run.jobs ?? []).length !== 0)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `a mining workflow planned ${run.jobs.length} job(s) — it must be refused before anything is dispatched`,
    });
  const why = run.error ?? '';
  // Verbatim: the sentence is the product's whole answer, and the hint
  // has to name what gave the line away.
  for (const want of ['mining software is not permitted on hosted runners', ':9', 'mining pool address']) {
    if (!why.includes(want))
      problems.push({
        where: stage,
        kind: 'content',
        text: `the refusal does not say ${JSON.stringify(want)}: ${JSON.stringify(why)}`,
      });
  }
  console.log(`  refused: ${why}`);

  // On the commit, as a failing check named for the file.
  await page.goto(`${BASE}/acme/${WF_REPO}/commit/${sha}`, { waitUntil: 'networkidle' });
  await page.getByText('.weft/ci.yml').first().waitFor({ timeout: 20000 });
  const row = await wfCheckRow('.weft/ci.yml', { strip: true });
  if (!row)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: 'a refused mining workflow leaves no check on the commit',
    });
  else if (row.state !== 'Failing')
    problems.push({
      where: stage,
      kind: 'content',
      text: `the refused mining workflow's check reads ${row.state ?? 'no state'}, not Failing: ${row.text}`,
    });
  wfExpectNoDuration(row, 'the refused mining workflow');
  await shot(page, '16l-workflow-miner-refused', 'a miner named in a workflow, refused at push time');
});

// ---------------------------------------------------------------------
// A fork's change. A stranger's `run:` lines do not get a machine until
// a maintainer says so — GitHub's "Approve and run", per tip.
// ---------------------------------------------------------------------
const FORK_KEY = `I${(Date.now() + 3).toString(16).padStart(12, '0')}`;
const FORK_SENTENCE =
  'this change comes from a fork; a maintainer has to approve its workflows before they run';
let forkChangeOpen = false;

await step("workflows / a stranger's change from a fork is held until a maintainer approves it", async () => {
  if (!wfLive) return;
  if (!MAIL_DIR) {
    problems.push({
      where: stage,
      kind: 'harness',
      text: 'no STRATUM_MAIL_DIR, so the newcomer never confirmed and cannot fork — the fork-approval stages prove nothing',
    });
    return;
  }
  // The repository has to be readable by a stranger before one can fork
  // it. Done through the API by the owner: it is a precondition, not
  // the thing under test — "describing a repository and publishing it"
  // below walks the form.
  const published = await page.evaluate(async (repo) => {
    const r = await fetch(`/v1/orgs/acme/repos/${repo}`, {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ public: true }),
    });
    return r.status;
  }, WF_REPO);
  if (published !== 200) {
    problems.push({
      where: stage,
      kind: 'harness',
      text: `publishing acme/${WF_REPO} answered ${published}; a stranger cannot fork a private repository`,
    });
    return;
  }

  // The stranger, in their own context: a different person, on a
  // different machine, who signed up at the top of this file.
  const forkerCtx = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const forker = await forkerCtx.newPage();
  watch(forker, () => stage);
  try {
    await forker.goto(`${BASE}/dashboard/`, { waitUntil: 'networkidle' });
    await forker.getByLabel('Email').fill(NEWCOMER_EMAIL);
    await forker.getByLabel('Password').fill('a long enough password');
    await forker.getByRole('button', { name: 'Sign in', exact: true }).click();
    await forker.getByText('Requests today').waitFor({ timeout: 15000 });

    // Fork, wait for the objects to land, commit a contribution on a
    // branch off trunk — so it carries the workflow file with it — and
    // open the change against the upstream, naming the fork.
    const forked = await forker.evaluate(
      async ({ repo, me, key }) => {
        const j = async (method, url, body) => {
          const r = await fetch(url, {
            method,
            headers: body ? { 'Content-Type': 'application/json' } : {},
            body: body ? JSON.stringify(body) : undefined,
          });
          return { status: r.status, body: await r.json().catch(() => null) };
        };
        const f = await j('POST', `/v1/orgs/acme/repos/${repo}/forks`, null);
        if (f.status !== 202) return { failed: `fork answered ${f.status}: ${JSON.stringify(f.body)}` };
        const until = Date.now() + 20000;
        for (;;) {
          const s = await j('GET', `/v1/orgs/${me}/repos/${repo}`);
          if (s.body?.fork_state === 'ready') break;
          if (s.body?.fork_state === 'failed' || Date.now() > until)
            return { failed: `fork never became ready: ${JSON.stringify(s.body)}` };
          await new Promise((r) => setTimeout(r, 250));
        }
        const b = await j('POST', `/v1/orgs/${me}/repos/${repo}/branches`, { name: 'contrib', from: 'main' });
        if (b.status !== 201) return { failed: `branch answered ${b.status}: ${JSON.stringify(b.body)}` };
        const c = await j('POST', `/v1/orgs/${me}/repos/${repo}/commits`, {
          message: `a contribution from a stranger\n\nChange-Id: ${key}\n`,
          branch: 'contrib',
          operations: [{ op: 'put', path: 'CONTRIB.md', content: 'hello from a fork\n' }],
        });
        if (c.status !== 201) return { failed: `commit answered ${c.status}: ${JSON.stringify(c.body)}` };
        return { sha: c.body.commit };
      },
      { repo: WF_REPO, me: NEWCOMER, key: FORK_KEY },
    );
    if (forked.failed) {
      problems.push({ where: stage, kind: 'behaviour', text: `the stranger could not prepare a contribution: ${forked.failed}` });
      return;
    }
    forkSha = forked.sha;

    // The review is opened through the form a stranger sees. For
    // somebody who cannot write, the fork field is the only route in,
    // so it has to be open already rather than behind a disclosure.
    await forker.goto(`${BASE}/acme/${WF_REPO}/changes`, { waitUntil: 'networkidle' });
    const forkField = forker.getByLabel('Fork the commits are in');
    if (!(await forkField.isVisible().catch(() => false))) {
      problems.push({
        where: stage,
        kind: 'content',
        text: 'the fork field is not open for a reader who cannot push — for them it is the only way to open a change',
      });
      return;
    }
    await forkField.fill(`${NEWCOMER}/${WF_REPO}`);
    await forker.getByLabel('Branch to review').fill('contrib');
    await shot(forker, '16m-fork-open', 'a stranger opening a change from their fork');
    await forker.getByRole('button', { name: 'Start review' }).click();
    await forker.getByRole('button', { name: FORK_KEY }).or(forker.getByText(FORK_KEY)).first().waitFor({ timeout: 20000 });
    forkChangeOpen = true;
  } finally {
    await forkerCtx.close();
  }

  // Held, not run: the run is `blocked`, nothing was dispatched, and the
  // sentence says whose decision it is waiting on.
  const run = await waitForRun(
    'was held for the fork change',
    (r) => r.commit_sha === forkSha && r.event === 'change' && r.state !== 'running',
  );
  if (!run) return;
  if (run.state !== 'blocked')
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `a fork's change run is ${run.state}, not blocked: ${run.error ?? ''}`,
    });
  if (run.error !== FORK_SENTENCE)
    problems.push({
      where: stage,
      kind: 'content',
      text: `the fork refusal is not the sentence trigger.rs settles with: ${JSON.stringify(run.error)}`,
    });
  if ((run.jobs ?? []).length !== 0)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `a fork's blocked run has ${run.jobs.length} job(s); nothing should have been planned`,
    });

  // The refusal is visible where a maintainer actually is — on the
  // change — verbatim, and the check row reads Blocked rather than
  // Queued. The mirror writes `queued` deliberately, so this is the join
  // working: without it the row is pixel-for-pixel a build about to
  // start.
  // Named for the file, not `ci / test`: nothing was planned, so there
  // is no job to name a row after, and the mirror writes one row per
  // workflow file the way it does for a file that does not parse.
  await page.goto(`${BASE}/acme/${WF_REPO}/changes/${FORK_KEY}`, { waitUntil: 'networkidle' });
  await page.getByText('.weft/ci.yml').first().waitFor({ timeout: 20000 });
  if (!(await page.getByText(FORK_SENTENCE).first().isVisible().catch(() => false)))
    problems.push({
      where: stage,
      kind: 'content',
      text: `the fork refusal is not on the change page. Expected verbatim: "${FORK_SENTENCE}"`,
    });
  const row = await wfCheckRow('.weft/ci.yml', { strip: true });
  if (!row)
    problems.push({ where: stage, kind: 'behaviour', text: 'no check row for the held run on the change page' });
  else if (row.state !== 'Blocked')
    problems.push({
      where: stage,
      kind: 'content',
      text: `the held run's check row reads ${row.state ?? 'no state'}, not Blocked: ${row.text}`,
    });
  wfExpectNoDuration(row, 'the held run');
  await shot(page, '16n-fork-blocked', "a fork's workflows, waiting on a person");

  // And the row is a way in: the run's own page carries the reason in
  // full and says, in words, that no job ever existed.
  const toRun = await wfFollowRunLink(run.id);
  if (!toRun) {
    problems.push({ where: stage, kind: 'behaviour', text: `the held run's check row has no link to run ${run.id}` });
    return;
  }
  if (toRun.popup)
    problems.push({ where: stage, kind: 'behaviour', text: "a hosted run's link opens a new tab; it is our own page" });
  const shown = (await toRun.page.innerText('body')).replace(/\s+/g, ' ');
  for (const want of [FORK_SENTENCE, 'This run never started a job. The reason is above.']) {
    if (!shown.includes(want))
      problems.push({
        where: stage,
        kind: 'content',
        text: `the blocked run's page does not say ${JSON.stringify(want)}; it shows: ${shown.slice(0, 600)}`,
      });
  }
  await shot(toRun.page, '16o-fork-blocked-run', 'a blocked run, and why');
  if (toRun.popup) await toRun.popup.close();
});

await step("workflows / a maintainer approves the fork's workflows, and they run", async () => {
  if (!wfLive || !forkChangeOpen) return;
  await page.goto(`${BASE}/acme/${WF_REPO}/changes/${FORK_KEY}`, { waitUntil: 'networkidle' });
  const arm = page.getByRole('button', { name: 'Approve and run workflows' });
  if (!(await arm.isVisible({ timeout: 15000 }).catch(() => false))) {
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: 'no "Approve and run workflows" button for the owner on a held change',
    });
    return;
  }
  await arm.click();
  // Two steps, deliberately: a control that runs a stranger's code on
  // our runners does not fire on a stray click, and the second step
  // names what is being agreed to.
  await page.getByText('This runs code from a fork on our runners.').waitFor({ timeout: 5000 });
  await shot(page, '16p-approve-confirm', 'the second, deliberate step');
  await page.getByRole('button', { name: 'Run them' }).click();

  // The panel goes away because the reason it was there did. Waited on
  // rather than slept on: it is the observable the next assertion needs.
  const gone = await page
    .getByText(/waiting for approval/)
    .waitFor({ state: 'detached', timeout: 20000 })
    .then(() => true)
    .catch(() => false);
  if (!gone)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: 'the approval panel is still there after approving — the runs were not re-read, or the server did not start them',
    });

  // The blocked placeholder is replaced by a real run for the same
  // commit, and it builds: the runner fetches the transplanted tip
  // (`refs/patchsets/<sha>`), which is how a fork's commit is reachable
  // from the upstream at all.
  const run = await waitForRun(
    'built the approved fork change',
    (r) => r.commit_sha === forkSha && r.event === 'change' && r.state !== 'running' && r.state !== 'blocked',
  );
  if (!run) return;
  if (run.state !== 'passed')
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `the approved fork run is ${run.state}: ${run.error ?? ''} — jobs ${JSON.stringify(
        (run.jobs ?? []).map((j) => [j.key, j.state, j.error]),
      )}`,
    });
  const still = (await wfRuns()).filter((r) => r.commit_sha === forkSha && r.state === 'blocked');
  if (still.length)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `${still.length} blocked run(s) survive the approval; the placeholder should have been replaced`,
    });
  // The change page reads its checks once and polls only while landing,
  // so the verdict is read the way a person reads it: by coming back.
  await page.goto(`${BASE}/acme/${WF_REPO}/changes/${FORK_KEY}`, { waitUntil: 'networkidle' });
  await page.getByText('All checks have passed').waitFor({ timeout: 30000 }).catch(() =>
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: 'the change page never reached "All checks have passed" after the approved run passed',
    }),
  );
  await shot(page, '16q-fork-approved', "the fork's workflows, run and green");

  // Pressing it twice is a 409, and a 409 is not a failure: somebody got
  // there first. The page says so neutrally.
  const twice = await page.evaluate(
    async ({ repo, key }) =>
      (await fetch(`/v1/orgs/acme/repos/${repo}/changes/${key}/workflows/approve`, { method: 'POST' })).status,
    { repo: WF_REPO, key: FORK_KEY },
  );
  if (twice !== 409)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `approving a tip that has nothing blocked answered ${twice}, not 409`,
    });
  console.log(`  approved fork run ${run.id} passed; a second approval is 409`);
});

// ---------------------------------------------------------------------
// Self-hosted runners. An organisation's own machine takes the jobs a
// file asks for with `runs-on: [self-hosted, …]`, shaped like GitHub's:
// an org policy for each pool, runner groups that admit repositories,
// a registration token exchanged for the runner's own credential, and a
// runner that only ever calls out.
//
// The runner is the REAL `weft-runner` binary, started by this stage
// from the command the settings page shows — not a stub, and not a
// runner the stack pre-registered. Registering a runner and seeing it in
// a table proves the form; a job that a person's own machine took and
// reported is the thing. `scripts/manual-stack.sh` exports RUNNER_BIN;
// without it these stages report the missing prerequisite, the same
// rule as a missing ECS stand-in.
// ---------------------------------------------------------------------
const RUNNER_BIN = process.env.RUNNER_BIN ?? null;
const SH_NAME = `walk-runner-${process.pid}`;
const SH_LABEL = 'walk';
const SH_DIR = fs.mkdtempSync(path.join(os.tmpdir(), 'walk-runner-'));
let shLive = false;
let shProc = null;
let shOut = '';
let shExit = null;

// A file for the organisation's own machine. It says what it ran on so a
// green run means a job really reached the runner this stage started.
function shWorkflow(labels, run = 'echo "job=$WEFT_JOB on $(hostname)"') {
  return `name: ci
on: [push]
jobs:
  own:
    runs-on: [${labels.join(', ')}]
    steps:
      - name: Where
        run: ${run}
      - name: Files
        run: test -f README.md
`;
}

function shPush(message, labels, run) {
  const branch = `wf-self-hosted-${Date.now()}`;
  wfGit('checkout', '--quiet', WF_BRANCH);
  wfGit('checkout', '--quiet', '-b', branch);
  const sha = wfCommit(message, shWorkflow(labels, run));
  wfGit('push', '--quiet', 'origin', branch);
  return sha;
}

// What the product says about acme's runners, under the signed-in
// session, the way the settings page reads it.
async function shRunners() {
  return page.evaluate(async () => {
    const r = await fetch('/v1/orgs/acme/runners');
    return r.ok ? ((await r.json()).runners ?? []) : [];
  });
}

async function shPolicy(patch) {
  return page.evaluate(async (body) => {
    const r = await fetch('/v1/orgs/acme/runner-policy', {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    });
    return { status: r.status, body: await r.json().catch(() => null) };
  }, patch);
}

// A settled run for `sha`, asserted against the sentence the product
// promised, and its row on the commit page read for state and — per the
// rule the refused-file stages taught this pass — for a duration it
// must not have.
async function shExpectRefused(sha, sentence, shotName, note) {
  const run = await waitForRun(
    `settled for ${sha.slice(0, 8)}`,
    (r) => r.commit_sha === sha && r.state !== 'running' && r.state !== 'queued',
    90000,
  );
  if (!run) return null;
  if (run.state !== 'failed' || run.error !== sentence)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `expected this push to be refused with "${sentence}"; the run is ${run.state}: ${JSON.stringify(run.error)} jobs ${JSON.stringify(
        (run.jobs ?? []).map((j) => [j.key, j.state, j.error]),
      )}`,
    });
  if (run.blocked_reason)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `a refusal a person has to fix carries blocked_reason ${JSON.stringify(run.blocked_reason)}; nothing lifts it by itself`,
    });
  await page.goto(`${BASE}/acme/${WF_REPO}/commit/${sha}`, { waitUntil: 'networkidle' });
  await page.getByText('.weft/ci.yml').first().waitFor({ timeout: 20000 }).catch(() => null);
  const row = await wfCheckRow('.weft/ci.yml', { strip: true });
  if (!row || row.state !== 'Failing')
    problems.push({
      where: stage,
      kind: 'content',
      text: `the commit's check row should read Failing; it reads ${row?.state ?? 'nothing'}: ${row?.text ?? ''}`,
    });
  wfExpectNoDuration(row, 'a refused self-hosted workflow');
  // The row names the file; the sentence is behind Details, on the run
  // page — and a refused run has no log, so it is that page's whole
  // explanation.
  await page.goto(`${BASE}/acme/${WF_REPO}/checks/runs/${run.id}`, { waitUntil: 'networkidle' });
  await page.getByText(sentence).first().waitFor({ timeout: 15000 }).catch(() => null);
  if (!(await page.getByText(sentence).count()))
    problems.push({
      where: stage,
      kind: 'content',
      text: `the run page does not show the refusal "${sentence}"`,
    });
  await shot(page, shotName, note);
  return run;
}

// The policy pickers are Radix selects — a button with role=combobox
// and a listbox of options — not <select> elements, so they are driven
// the way a person drives them.
async function shPick(label, option) {
  await page.getByRole('combobox', { name: label }).click();
  await page.getByRole('option', { name: option, exact: true }).click();
}

async function shSettings() {
  await page.goto(`${BASE}/dashboard/settings/runners`, { waitUntil: 'networkidle' });
  // Panel titles are CardTitle divs, not headings — match the text.
  await page.getByText('Runner policy', { exact: true }).first().waitFor({ timeout: 15000 });
}

await step('runners / an organisation can see its runner policy, and has no runners yet', async () => {
  if (!wfLive) return;
  if (!RUNNER_BIN || !fs.existsSync(RUNNER_BIN)) {
    problems.push({
      where: stage,
      kind: 'harness',
      text: `no RUNNER_BIN (${RUNNER_BIN ?? 'unset'}) — the self-hosted stages need the real weft-runner binary; scripts/manual-stack.sh exports it when target/release/weft-runner exists (cargo build --release -p stratum-runner)`,
    });
    console.log('  no runner binary — skipping the self-hosted loop');
    return;
  }
  await shSettings();
  const text = (await page.innerText('body')).replace(/\s+/g, ' ');
  for (const want of ['Weft-hosted runners', 'Self-hosted runners', 'Runner groups', 'Add a runner'])
    if (!text.includes(want))
      problems.push({ where: stage, kind: 'content', text: `the Runners settings page does not show "${want}"` });
  const existing = await shRunners();
  if (existing.length)
    problems.push({
      where: stage,
      kind: 'harness',
      text: `acme already has ${existing.length} runner(s) (${existing.map((r) => r.name).join(', ')}) — a stack a previous run left behind; bring it up fresh`,
    });
  await shot(page, '16v-runners-settings', 'the policy for both pools, one default group, no runners');
  shLive = true;
});

await step('runners / a public repository is outside every group until an owner lets it in', async () => {
  if (!shLive) return;
  // acme/builds went public for the fork stages above, and the default
  // group does not admit public repositories: a stranger's change would
  // otherwise run on the organisation's own machine. The refusal names
  // the fix.
  const sha = shPush('build on our own machine', ['self-hosted', SH_LABEL]);
  const run = await shExpectRefused(
    sha,
    'no runner group admits this repository; add it to a group under Settings → Runners',
    '16w-public-repo-not-admitted',
    'a public repository, refused by the default group',
  );
  if (!run) return;
  // Let it in, from the group's own row.
  await shSettings();
  const group = page.getByRole('table').first().getByRole('row').filter({ hasText: 'default' }).first();
  await group.getByRole('button', { name: 'Edit' }).click();
  await page.getByRole('checkbox', { name: 'Allow public repositories' }).check();
  await page.getByRole('button', { name: 'Save group' }).click();
  await page.getByRole('status').filter({ hasText: 'Saved' }).first().waitFor({ timeout: 10000 }).catch(() => null);
  if (!(await page.getByRole('status').filter({ hasText: 'Saved' }).count()))
    problems.push({ where: stage, kind: 'content', text: 'saving a group shows no "Saved" confirmation' });
  const groups = await page.evaluate(async () => (await fetch('/v1/orgs/acme/runner-groups')).json());
  const dflt = (groups.groups ?? []).find((g) => g.is_default);
  if (!dflt?.allow_public)
    problems.push({ where: stage, kind: 'behaviour', text: `saving the default group with public repositories allowed did not stick: ${JSON.stringify(dflt)}` });
  await shot(page, '16x-group-allows-public', 'the default group now admits public repositories');
  console.log(`  run ${run.id} refused; default group now allows public repositories`);
});

await step('runners / a job for a runner nobody has registered is refused, not queued forever', async () => {
  if (!shLive) return;
  const sha = shPush('build on our own machine', ['self-hosted', SH_LABEL]);
  const run = await shExpectRefused(
    sha,
    `no runner with labels [self-hosted, ${SH_LABEL}] is registered for this repository`,
    '16y-no-runner-registered',
    'the labels nobody offers, named in the refusal',
  );
  if (run) console.log(`  run ${run.id} refused: ${run.error}`);
});

await step('runners / a runner is registered with the command the page shows', async () => {
  if (!shLive) return;
  await shSettings();
  await page.getByRole('button', { name: 'Add a runner' }).click();
  const block = page.getByLabel('Runner registration commands');
  await block.waitFor({ timeout: 10000 });
  const shown = (await block.innerText()).trim();
  await shot(page, '16z-registration-command', 'a single-use registration token, shown once');
  const m = shown.match(/weft-runner register --url (\S+) --token (\S+)/);
  if (!m) {
    problems.push({ where: stage, kind: 'content', text: `the registration block does not carry a usable command: ${shown}` });
    return;
  }
  const [, url, token] = m;
  if (url !== BASE)
    problems.push({ where: stage, kind: 'content', text: `the command points the runner at ${url}, not the origin the page is on (${BASE})` });
  if (!shown.includes('weft-runner run'))
    problems.push({ where: stage, kind: 'content', text: `the block shows how to register but not how to run: ${shown}` });
  if (!/This token expires in 60 minutes/.test((await page.innerText('body')).replace(/\s+/g, ' ')))
    problems.push({ where: stage, kind: 'content', text: 'the registration token does not say when it expires' });

  // The command, run as shown — plus a name and a label so the routing
  // below is about THIS runner.
  let registered;
  try {
    registered = execFileSync(
      RUNNER_BIN,
      ['register', '--url', url, '--token', token, '--name', SH_NAME, '--labels', SH_LABEL, '--dir', SH_DIR],
      { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] },
    ).trim();
  } catch (e) {
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `weft-runner register failed: ${`${e.stderr ?? ''}${e.stdout ?? ''}`.trim() || e.message}`,
    });
    return;
  }
  console.log(`  ${registered}`);
  if (!fs.existsSync(path.join(SH_DIR, '.runner')))
    problems.push({ where: stage, kind: 'behaviour', text: 'register printed success but wrote no .runner file' });

  // The same token, twice, is refused: it was single-use.
  let twice = 'accepted';
  try {
    execFileSync(RUNNER_BIN, ['register', '--url', url, '--token', token, '--name', `${SH_NAME}-again`, '--dir', fs.mkdtempSync(path.join(os.tmpdir(), 'walk-runner-x-'))], { stdio: ['ignore', 'pipe', 'pipe'] });
  } catch (e) {
    twice = `${e.stderr ?? ''}${e.stdout ?? ''}`.trim();
  }
  if (twice === 'accepted')
    problems.push({ where: stage, kind: 'behaviour', text: 'a registration token registered two runners; it is meant to be single-use' });

  shProc = spawn(RUNNER_BIN, ['run', '--dir', SH_DIR], { stdio: ['ignore', 'pipe', 'pipe'] });
  shProc.stdout.on('data', (d) => { shOut += d; });
  shProc.stderr.on('data', (d) => { shOut += d; });
  shProc.on('exit', (code) => { shExit = code; });

  // It appears in the table as online, on its own — the runner's first
  // claim is what says it is alive.
  const deadline = Date.now() + 30000;
  let row = null;
  while (Date.now() < deadline) {
    const list = await shRunners();
    row = list.find((r) => r.name === SH_NAME) ?? null;
    if (row && row.state === 'online') break;
    await new Promise((r) => setTimeout(r, 750));
  }
  if (!row || row.state !== 'online') {
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `the runner never showed as online; the list has ${JSON.stringify(row)}; the runner said: ${shOut.slice(0, 400)}`,
    });
    return;
  }
  for (const l of ['self-hosted', SH_LABEL])
    if (!row.labels.includes(l))
      problems.push({ where: stage, kind: 'behaviour', text: `the runner's labels ${JSON.stringify(row.labels)} lack ${l}` });
  if (!row.labels.some((l) => ['linux', 'macos', 'windows'].includes(l)) || !row.labels.some((l) => ['x64', 'arm64'].includes(l)))
    problems.push({ where: stage, kind: 'behaviour', text: `the server did not add the OS and architecture labels: ${JSON.stringify(row.labels)}` });
  await shSettings();
  const ui = page.getByRole('table').nth(1).getByRole('row').filter({ hasText: SH_NAME }).first();
  await ui.waitFor({ timeout: 10000 });
  const uiText = (await ui.innerText()).replace(/\s+/g, ' ');
  if (!/\bOnline\b/.test(uiText))
    problems.push({ where: stage, kind: 'content', text: `the runner's row does not say it is online: ${uiText}` });
  await shot(page, '16aa-runner-online', "the organisation's own machine, listening");
  console.log(`  ${SH_NAME} is online with labels [${row.labels.join(', ')}]`);
});

await step("runners / a push runs on the organisation's own machine, and costs no minutes", async () => {
  if (!shLive || !shProc) return;
  const before = await page.evaluate(async () => (await fetch('/v1/orgs/acme/billing')).json());
  const sha = shPush('build on our own machine', ['self-hosted', SH_LABEL]);
  const run = await waitForRun(
    'reported from the self-hosted runner',
    (r) => r.commit_sha === sha && r.state !== 'running' && r.state !== 'queued',
    120000,
  );
  if (!run) return;
  const job = (run.jobs ?? [])[0];
  if (run.state !== 'passed')
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `the self-hosted run is ${run.state}: ${run.error ?? ''} jobs ${JSON.stringify((run.jobs ?? []).map((j) => [j.key, j.state, j.error]))}; runner said: ${shOut.slice(-400)}`,
    });
  if (job?.pool !== 'self_hosted' || job?.runner?.name !== SH_NAME)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `the job should record pool self_hosted on ${SH_NAME}; it records ${JSON.stringify({ pool: job?.pool, runner: job?.runner })}`,
    });
  // The run page names the machine.
  await page.goto(`${BASE}/acme/${WF_REPO}/checks/runs/${run.id}`, { waitUntil: 'networkidle' });
  const body = (await page.innerText('body')).replace(/\s+/g, ' ');
  if (!body.includes(`ran on ${SH_NAME}`))
    problems.push({ where: stage, kind: 'content', text: `the run page does not say the job ran on ${SH_NAME}` });
  const log = page.getByRole('log', { name: 'Build log' });
  await log.waitFor({ timeout: 20000 }).catch(() => null);
  const logText = (await log.innerText().catch(() => '')).replace(/\s+/g, ' ');
  if (!logText.includes(`on ${os.hostname()}`))
    problems.push({ where: stage, kind: 'content', text: `the log should carry this machine's hostname (${os.hostname()}); it shows: ${logText.slice(0, 300)}` });
  await shot(page, '16ab-self-hosted-run', 'a job the organisation ran on its own machine');

  // Not metered: the organisation's own hardware costs it nothing here.
  const after = await page.evaluate(async () => (await fetch('/v1/orgs/acme/billing')).json());
  if (after.ci_minutes_used !== before.ci_minutes_used)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `a self-hosted job was metered: ${before.ci_minutes_used} → ${after.ci_minutes_used} minutes used`,
    });
  console.log(`  run ${run.id} passed on ${SH_NAME}; minutes used ${before.ci_minutes_used} → ${after.ci_minutes_used}`);
});

await step('runners / a label no runner offers is refused, and the refusal names the labels', async () => {
  if (!shLive) return;
  const sha = shPush('a build that wants a gpu', ['self-hosted', 'gpu']);
  const run = await shExpectRefused(
    sha,
    'no runner with labels [self-hosted, gpu] is registered for this repository',
    '16ac-label-nobody-offers',
    'the label nobody offers, named',
  );
  if (run) console.log(`  run ${run.id} refused: ${run.error}`);
});

await step('runners / an owner can turn either pool off, and a file that asks for it is told', async () => {
  if (!shLive) return;
  await shSettings();
  // Self-hosted off, from the page.
  await shPick('Self-hosted runners', 'Disabled');
  await page.getByRole('button', { name: 'Save policy' }).click();
  await page.getByRole('status').filter({ hasText: 'Saved' }).waitFor({ timeout: 10000 });
  await shot(page, '16ad-self-hosted-disabled', 'self-hosted runners turned off for the organisation');
  const sha = shPush('build on our own machine', ['self-hosted', SH_LABEL]);
  await shExpectRefused(
    sha,
    'self-hosted runners are not allowed for this repository (organisation policy)',
    '16ae-refused-by-policy',
    'a file asking for a pool the organisation turned off',
  );
  // Hosted off; our own runners back on.
  await shSettings();
  await shPick('Self-hosted runners', 'All repositories');
  await shPick('Weft-hosted runners', 'Disabled');
  await page.getByRole('button', { name: 'Save policy' }).click();
  await page.getByRole('status').filter({ hasText: 'Saved' }).waitFor({ timeout: 10000 });
  await shot(page, '16ae2-hosted-disabled', 'an organisation that only trusts its own machines');
  const branch = `wf-hosted-off-${Date.now()}`;
  wfGit('checkout', '--quiet', WF_BRANCH);
  wfGit('checkout', '--quiet', '-b', branch);
  const hostedSha = wfCommit('an honest hosted build, with hosted runners off', WF_FAST);
  wfGit('push', '--quiet', 'origin', branch);
  await shExpectRefused(
    hostedSha,
    'hosted runners are disabled for this organisation; use runs-on: [self-hosted, …]',
    '16af-hosted-refused-by-policy',
    'a file asking for our runners, in an organisation that only trusts its own',
  );
  // And back, or every hosted stage below reads as broken.
  const restored = await shPolicy({ hosted: 'allowed', self_hosted: 'all' });
  if (restored.status !== 200 || restored.body?.hosted !== 'allowed' || restored.body?.self_hosted !== 'all')
    problems.push({ where: stage, kind: 'behaviour', text: `restoring the policy answered ${restored.status}: ${JSON.stringify(restored.body)}` });
  console.log('  both pools refused by policy in turn; policy restored');
});

await step("runners / a miner on the organisation's own machine is stopped, and nobody is suspended", async () => {
  if (!shLive || !shProc) return;
  // The same renamed program as the hosted stage below. On our machine
  // that costs the organisation its hosted runners; on theirs it is
  // their hardware, and the watch is protecting them from a stranger's
  // change — so the job stops, with the same sentence, and that is all.
  const sha = shPush(
    'a build that turns into a miner, on our own machine',
    ['self-hosted', SH_LABEL],
    '|\n          m=./xm; ln -s /bin/sleep "${m}rig"\n          "${m}rig" 120',
  );
  const run = await waitForRun(
    'stopped the self-hosted miner',
    (r) => r.commit_sha === sha && r.state !== 'running' && r.state !== 'queued',
    180000,
  );
  if (!run) return;
  const why = (run.jobs ?? []).map((j) => j.error ?? '').find((e) => e) ?? '';
  if (run.state !== 'failed' || !why.includes('mining software detected: xmrig'))
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `the self-hosted miner's run is ${run.state}, jobs ${JSON.stringify((run.jobs ?? []).map((j) => [j.key, j.state, j.error]))} — expected failed with "mining software detected: xmrig"; runner said: ${shOut.slice(-400)}`,
    });
  const bill = await page.evaluate(async () => (await fetch('/v1/orgs/acme/billing')).json());
  if (bill.ci_suspended_reason)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `a miner on the organisation's own machine suspended its hosted CI: ${JSON.stringify(bill.ci_suspended_reason)}`,
    });
  // The runner itself is still listening: it killed the job, not itself.
  const still = (await shRunners()).find((r) => r.name === SH_NAME);
  if (shExit !== null || !still || still.state === 'offline')
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `the runner did not survive stopping a miner (exit ${shExit}, listed as ${JSON.stringify(still?.state)}); it said: ${shOut.slice(-400)}`,
    });
  await page.goto(`${BASE}/acme/${WF_REPO}/checks/runs/${run.id}`, { waitUntil: 'networkidle' });
  await shot(page, '16ag-self-hosted-miner-stopped', 'stopped on their machine; their hosted minutes untouched');
  console.log(`  run ${run.id} ${run.state}: ${why}; acme not suspended`);
});

// ---------------------------------------------------------------------
// The miner that walks past layer 2. `curl | sh`, a renamed binary, a
// name assembled at runtime — the parser cannot see it, and is not
// meant to. The runner watches what actually runs.
// ---------------------------------------------------------------------
const SUSPENDED_SENTENCE =
  'hosted workflows are suspended for this organisation: mining software detected: xmrig';

await step('workflows / a miner that starts at runtime is killed, and the organisation is suspended', async () => {
  if (!wfLive) return;
  const branch = `wf-runtime-miner-${Date.now()}`;
  wfGit('checkout', '--quiet', WF_BRANCH);
  wfGit('checkout', '--quiet', '-b', branch);
  // The program is /bin/sleep under a miner's name, assembled so that no
  // line names it — which is exactly the file layer 2 lets through.
  const sha = wfCommit(
    'a build that turns into a miner',
    'name: ci\non: [push]\njobs:\n  build:\n    steps:\n      - name: Miner\n        run: |\n          m=./xm; ln -s /bin/sleep "${m}rig"\n          "${m}rig" 120\n',
  );
  wfGit('push', '--quiet', 'origin', branch);

  const run = await waitForRun(
    'stopped the runtime miner',
    (r) => r.commit_sha === sha && r.state !== 'running' && r.state !== 'queued',
    180000,
  );
  if (!run) return;
  // The sentence is the job's: a run that a job failed carries no error
  // of its own (that field is for a run that never got as far as a job).
  const why = (run.jobs ?? []).map((j) => j.error ?? '').find((e) => e) ?? '';
  if (run.state !== 'failed' || !why.includes('mining software detected: xmrig'))
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `the runtime miner's run is ${run.state}, jobs ${JSON.stringify(
        (run.jobs ?? []).map((j) => [j.key, j.state, j.error]),
      )} — expected failed with "mining software detected: xmrig"`,
    });
  console.log(`  run ${run.id} ${run.state}: ${why}`);

  // The log says which step was stopped and why, in the same words.
  await page.goto(`${BASE}/acme/${WF_REPO}/checks/runs/${run.id}`, { waitUntil: 'networkidle' });
  const log = page.getByRole('log', { name: 'Build log' });
  await log.waitFor({ timeout: 20000 }).catch(() => null);
  const logText = (await log.innerText().catch(() => '')).replace(/\s+/g, ' ');
  if (!/Miner stopped: mining software detected: xmrig/.test(logText))
    problems.push({
      where: stage,
      kind: 'content',
      text: `the log does not say the step was stopped for mining; it shows: ${logText.slice(0, 500)}`,
    });
  await shot(page, '16r-runtime-miner-killed', 'a step stopped for what it was running');

  // The organisation is suspended, and says so where money is:
  // Settings → Billing.
  const bill = await page.evaluate(async () => (await fetch('/v1/orgs/acme/billing')).json());
  if (bill.ci_suspended_reason !== 'mining software detected: xmrig')
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `billing reports ci_suspended_reason ${JSON.stringify(bill.ci_suspended_reason)} after a miner was stopped`,
    });
  await page.goto(`${BASE}/dashboard/settings/billing`, { waitUntil: 'networkidle' });
  // A heading, not text: on a paid org the meter row and the panel
  // both carry the words, and a bare text match is a strict-mode error.
  await page.getByRole('heading', { name: 'Hosted CI minutes' }).waitFor({ timeout: 15000 }).catch(() => null);
  const billingText = (await page.innerText('body')).replace(/\s+/g, ' ');
  if (!billingText.includes('Hosted CI is suspended') || !billingText.includes('mining software detected: xmrig'))
    problems.push({
      where: stage,
      kind: 'content',
      text: `the billing page does not say hosted CI is suspended, or does not quote why; it shows: ${billingText.slice(0, 600)}`,
    });
  await shot(page, '16s-suspended', 'an organisation told its hosted CI is suspended, and why');

  // And every later push is refused with the reason — blocked, not
  // failed, because nothing about the code was judged.
  const after = `wf-after-suspension-${Date.now()}`;
  wfGit('checkout', '--quiet', WF_BRANCH);
  wfGit('checkout', '--quiet', '-b', after);
  const sha2 = wfCommit('an honest build, after the suspension', WF_FAST);
  wfGit('push', '--quiet', 'origin', after);
  const blocked = await waitForRun(
    'was blocked after the suspension',
    (r) => r.commit_sha === sha2 && r.state !== 'running' && r.state !== 'queued',
    90000,
  );
  if (!blocked) return;
  if (blocked.state !== 'blocked' || blocked.error !== SUSPENDED_SENTENCE)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `a push after the suspension settled as ${blocked.state}: ${JSON.stringify(blocked.error)} — expected blocked with "${SUSPENDED_SENTENCE}"`,
    });
  if ((blocked.jobs ?? []).length !== 0 || (await wfTasks()).some((t) => t.env?.STRATUM_JOB_ID && (blocked.jobs ?? []).some((j) => j.id === t.env.STRATUM_JOB_ID)))
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: 'a suspended organisation was given a machine',
    });
  await page.goto(`${BASE}/acme/${WF_REPO}/commit/${sha2}`, { waitUntil: 'networkidle' });
  await page.getByText('.weft/ci.yml').first().waitFor({ timeout: 20000 });
  const row = await wfCheckRow('.weft/ci.yml', { strip: true });
  if (!row || row.state !== 'Blocked' || !row.text.includes(SUSPENDED_SENTENCE))
    problems.push({
      where: stage,
      kind: 'content',
      text: `the post-suspension check row should read Blocked with the reason; it reads ${row?.state ?? 'nothing'}: ${row?.text ?? ''}`,
    });
  wfExpectNoDuration(row, 'the post-suspension run');
  await shot(page, '16t-blocked-after-suspension', 'the next push, refused with the reason');
  console.log('  acme is suspended; clearing it is an operator action (see docs/deployment-aws.md)');
});

await step("runners / the organisation's own machine keeps building while its hosted CI is suspended", async () => {
  if (!shLive || !shProc) return;
  // The suspension above is about OUR machines. Theirs owe us nothing,
  // and a file that only asks for theirs still runs.
  const sha = shPush('an honest build on our own machine, after the suspension', ['self-hosted', SH_LABEL]);
  const run = await waitForRun(
    'ran on the self-hosted runner after the suspension',
    (r) => r.commit_sha === sha && r.state !== 'running' && r.state !== 'queued',
    120000,
  );
  if (!run) return;
  if (run.state !== 'passed' || (run.jobs ?? [])[0]?.runner?.name !== SH_NAME)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `a self-hosted-only file after the suspension is ${run.state}: ${JSON.stringify(run.error)} jobs ${JSON.stringify((run.jobs ?? []).map((j) => [j.key, j.state, j.error, j.runner?.name]))}`,
    });
  await page.goto(`${BASE}/acme/${WF_REPO}/commit/${sha}`, { waitUntil: 'networkidle' });
  await page.getByText('ci / own').first().waitFor({ timeout: 20000 }).catch(() => null);
  const row = await wfCheckRow('ci / own', { strip: true });
  if (!row || row.state !== 'Passing')
    problems.push({ where: stage, kind: 'content', text: `the check row for the self-hosted run after the suspension reads ${row?.state ?? 'nothing'}: ${row?.text ?? ''}` });
  await shot(page, '16ah-self-hosted-during-suspension', 'their machine, still building');
  console.log(`  run ${run.id} passed on ${SH_NAME} while acme's hosted CI is suspended`);
});

await step('runners / removing a runner from the page ends it', async () => {
  if (!shLive || !shProc) return;
  await shSettings();
  const row = page.getByRole('table').nth(1).getByRole('row').filter({ hasText: SH_NAME }).first();
  await row.getByRole('button', { name: `Remove ${SH_NAME}` }).click();
  await shot(page, '16ai-remove-runner-confirm', 'removal asks once, in place, and says what it costs');
  await page.getByRole('button', { name: 'Remove the runner' }).click();
  await row.waitFor({ state: 'detached', timeout: 10000 }).catch(() => null);
  if ((await shRunners()).some((r) => r.name === SH_NAME))
    problems.push({ where: stage, kind: 'behaviour', text: 'the runner is still listed after Remove' });
  // The process finds out on its next call and stops on its own — no
  // signal from here. Its credential is dead; nothing it says is trusted.
  const deadline = Date.now() + 40000;
  while (shExit === null && Date.now() < deadline) await new Promise((r) => setTimeout(r, 500));
  if (shExit !== 2 || !shOut.includes('this runner has been removed'))
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `a removed runner should exit 2 saying so; exit ${shExit}, output: ${shOut.slice(-400)}`,
    });
  await shot(page, '16aj-runner-removed', 'gone from the list; the process stopped itself');
  console.log(`  ${SH_NAME} removed; the process exited ${shExit}`);
});

await step('settings / hosted CI minutes', async () => {
  if (!wfLive) return;
  await page.goto(`${BASE}/dashboard/settings/billing`, { waitUntil: 'networkidle' });
  const panel = page.getByRole('heading', { name: 'Hosted CI minutes' });
  if (!(await panel.isVisible().catch(() => false))) {
    // Not a defect on a deployment that does not meter — but said out
    // loud, because "no panel" and "no budget configured" look identical
    // and only one of them means this stage tested anything.
    console.log('  (no minutes panel: STRATUM_RUNNER_MINUTES_PER_MONTH is unset, so this stage asserted nothing)');
    return;
  }
  // Both figures, not just what is left: "760 left" alone says nothing
  // about whether that is most of the window or the last of it.
  const line = await page.getByText(/of .* minutes used (in the last 30 days|this period)/).first().isVisible().catch(() => false);
  if (!line)
    problems.push({ where: stage, kind: 'content', text: 'the minutes panel shows a remainder with no budget beside it' });
  await shot(page, '16u-ci-minutes', 'what is left of the 30-day window');
  // Over budget is worth doing by hand once — `UPDATE orgs SET
  // ci_minutes_per_month = 1` (zero means unlimited), push, read the
  // Checks tab and this panel
  // — because it needs a write to the control database, which this
  // walkthrough otherwise never does.
});

await step('workflows / hand the browser and the credential back', async () => {
  if (wfLive) {
    // Put the checkout back on the branch the stages above left it on, and
    // drop the credential this loop minted.
    wfGit('checkout', '--quiet', WF_BRANCH);
    const gone = await page.evaluate(async () => {
      const list = await (await fetch('/v1/orgs/acme/tokens')).json();
      // Live ones only, newest last — see the note in the ci stage above.
      const t = (list.tokens ?? [])
        .filter((x) => x.label === 'walkthrough-workflow-push' && !x.revoked_at)
        .pop();
      if (!t) return 'not listed';
      const r = await fetch(`/v1/orgs/acme/tokens/${t.id}`, { method: 'DELETE' });
      return r.status;
    });
    if (gone !== 204)
      problems.push({
        where: stage,
        kind: 'behaviour',
        text: `revoking the workflow push token answered ${JSON.stringify(gone)}`,
      });
    fs.rmSync(WF_DIR, { recursive: true, force: true });
    // The runner this pass started, if the removal stage did not already
    // end it, and its working directory. A runner left listening would
    // take the next pass's first self-hosted job under a name that pass
    // never registered.
    if (shProc && shExit === null) shProc.kill('SIGTERM');
    fs.rmSync(SH_DIR, { recursive: true, force: true });
  }

  // Hand the browser back the way the stages above found it. These
  // stages end on the forge mount, and everything after them is the
  // dashboard's own chrome — the first run left the page on a commit
  // page and the next three stages timed out looking for a Settings nav
  // that was not on it. A stage that moves the browser somewhere else
  // has to put it back.
  //
  // Unconditionally, and that is the point. This used to sit after an
  // early `if (!wfLive) return`, so on a stack with no hosted runner the
  // browser stayed on the forge's change page, `settings / password`
  // could not find its link, and four stages threw — the viewer's
  // sign-in among them, which left the owner signed in and turned the
  // viewer's checks into two false security problems.
  await page.goto(`${BASE}/dashboard/`, { waitUntil: 'networkidle' });
  await page.getByText('Requests today').waitFor({ timeout: 15000 });
});

await step('settings / password', async () => {
  await page.getByRole("link", { name: "Password" }).click();
  await shot(page, '17-password', 'changing a password ends every other session');
});

await step('sign out', async () => {
  await page.getByRole('button', { name: 'Sign out' }).click();
  await page.getByLabel('Email').waitFor({ timeout: 10000 });
  await shot(page, '18-signed-out', 'back to the sign-in form');
});

await step("sign in as viewer", async () => {
  await page.getByLabel("Email").fill("view@acme.dev");
  await page.getByLabel("Password").fill("a long enough password");
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  // The address survives sign-out, so signing in resumes wherever the
  // last person left the URL — their own view of it. Go home explicitly
  // before expecting the overview.
  await page.getByRole("link", { name: "Repositories" }).click();
  await page.getByText("Requests today").waitFor({ timeout: 15000 });
  await page.getByRole("link", { name: "Teams" }).click();
  await page.waitForTimeout(500);
  const members = await page.getByRole("link", { name: "Members" }).count();
  if (members !== 0)
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "a viewer is offered the Members tab",
    });
  await shot(
    page,
    "17-viewer-settings",
    "a viewer is offered only what they can act on",
  );
});

await step("viewer mints within their role", async () => {
  await page.getByRole("link", { name: "Tokens" }).click();
  const offered = await page.locator("form code").allTextContents();
  console.log(`  scopes offered to a viewer: ${offered.join(" ")}`);
  if (offered.includes("repo:write") || offered.includes("org:admin"))
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `a viewer is offered ${offered.join(" ")}`,
    });
  await page.getByLabel("Token label").fill("viewer-token");
  await page.getByRole("button", { name: "Mint token" }).click();
  await page.getByText(/^weft_/).waitFor({ timeout: 10000 });
  await shot(
    page,
    "18-viewer-token",
    "a viewer mints within their role, and no further",
  );
});

await step("repo view", async () => {
  await openRepo("payments-api");
  await page.getByRole("navigation", { name: "Repository" }).waitFor({ timeout: 10000 });
  // Wait for the **row**, not for the tab strip.
  //
  // Both tabs below are decided by `viewer_member` / `viewer_admin`, and
  // those arrive with the repository row, one request after the strip
  // has already painted. Counting at this moment reads a row still in
  // flight as "no Insights tab" — which is what this stage reported for
  // three runs while the server was answering `viewer_member: true` and
  // the page was rendering the tab correctly a few hundred milliseconds
  // later. The Settings check underneath had the same flaw pointing the
  // other way: an absence asserted before the answer arrives holds
  // trivially, so it would have passed against a build that gave a
  // viewer the Settings tab.
  //
  // `repo-settings.spec.ts` learned this and wrote `rowHasLanded` for
  // it. The visibility badge is this page's equivalent: it is drawn from
  // the row and from nothing else, and deliberately not guessed while
  // the read is in flight.
  await page
    .getByText(/^(Public|Private)$/)
    .first()
    .waitFor({ timeout: 10000 });
  // A viewer gets the repository — this is the same page a maintainer
  // sees, which is the whole design — and the two gates on it land on
  // opposite sides of them, which is the whole reason there are two.
  //
  // They are a **member**: Insights is theirs to read, because a
  // repository's traffic belongs to the people who own it. They are not
  // an admin: Settings is not.
  if (!(await repoTab("Insights").count()))
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "a member with the viewer role is not offered the Insights tab",
    });
  if (await repoTab("Settings").count())
    problems.push({
      where: stage,
      kind: "security",
      text: "a viewer is offered the Settings tab",
    });
  // The listing, not the word "Loading…". The row lands before the tree
  // does, so the wait above is enough to assert the tabs and not enough
  // to photograph the page — and this shot is one a person reads. The
  // `browsing the code` stage carries the same note for the same reason.
  await page
    .locator("table button")
    .first()
    .waitFor({ timeout: 15000 })
    .catch(() => undefined);
  await shot(page, "19-repo", "the repository as a viewer — Insights, no Settings");

  // The offer is one half; the address is the other, and only the
  // second is a leak. A viewer may read the numbers, so Insights must
  // *answer* — an assertion that only checked the refusal would pass
  // against a page that refused everybody.
  await page.goto(`${BASE}/acme/payments-api/insights`, { waitUntil: "networkidle" });
  if (!(await page.getByText("Clone p50 / p99").count()))
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "a member with the viewer role is refused the Insights page",
    });
  await page.goto(`${BASE}/acme/payments-api/settings`, { waitUntil: "networkidle" });
  await page.waitForTimeout(700);
  if (!(await page.getByText(/couldn.t find/i).count()))
    problems.push({
      where: stage,
      kind: "security",
      text: "a viewer who types /settings is shown the access map",
    });
});

// Making a repository, which until now was impossible from here at all,
// and mirroring one, which until now asked for an installation id dug
// out of a GitHub settings URL. Driven as the owner: a viewer cannot
// create, and the steps after this one sign in as one.
await step("signing back in as the owner", async () => {
  await page.goto(`${BASE}/dashboard/`, { waitUntil: "networkidle" });
  const out = await page.getByRole("button", { name: "Sign out" }).count();
  if (out) await page.getByRole("button", { name: "Sign out" }).click();
  await page.getByLabel("Email").waitFor({ timeout: 10000 });
  await page.getByLabel("Email").fill("ada@acme.dev");
  await page.getByLabel("Password").fill("a long enough password");
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  // Same resume-the-URL behavior as the viewer sign-in above.
  await page.getByRole("link", { name: "Repositories" }).click();
  await page.getByText("Requests today").waitFor({ timeout: 15000 });
});

await step("making an empty repository", async () => {
  await page.getByRole("button", { name: "New repository" }).click();
  await page.waitForURL(/\/dashboard\/new$/, { timeout: 15000 });
  await shot(page, "22-new-repo", "one screen, two ways in");

  await page.getByRole("tab", { name: "Empty repository" }).click();
  const name = `scratch-${Date.now()}`;
  await page.getByLabel("Repository name").fill(name);
  await page.getByRole("button", { name: "Create repository" }).click();
  // The clone command is the deliverable: a repository you cannot clone
  // is a row in a table.
  const cmd = page.getByText(/^git clone /);
  await cmd.waitFor({ timeout: 20000 }).catch(() => undefined);
  const shown = (await cmd.textContent().catch(() => "")) ?? "";
  if (!shown.includes(name)) {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `creating ${name} did not end on its clone command (got ${shown || "nothing"})`,
    });
  } else {
    console.log(`  created: ${shown.trim()}`);
  }
  await shot(page, "23-created", "created, and the command to clone it");
});

/// Whether the *server* has a GitHub App, asked of the server.
///
/// This used to read `process.env.STRATUM_GITHUB_INSTALL_URL` — the
/// runner's own environment — which is a proxy for the thing that
/// matters and disagrees with it in both directions. A correctly
/// configured stack driven from a shell that had not exported the
/// variable reported "no GitHub App configured" and failed a gate whose
/// threshold is zero problems; the mirror image would pass a stack that
/// has the variable and no working App. Ask the endpoint: it answers 501
/// when there is nothing configured, and a URL when there is.
async function githubConfigured(pg) {
  return await pg.evaluate(async (base) => {
    try {
      const r = await fetch(`${base}/v1/orgs/acme/github/install`, {
        method: "POST",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: "{}",
      });
      if (r.status === 501) return false;
      const body = await r.json();
      return typeof body.url === "string" && body.url.length > 0;
    } catch {
      return false;
    }
  }, BASE);
}

await step("mirroring by pasting a URL", async () => {
  if (!(await githubConfigured(page))) {
    // The same failure mode as the SSH front door: with no App
    // configured the connect flow is a screen nobody clicks, and the
    // pass silently becomes a walkthrough of a different product.
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "no GitHub App configured on the server — the connect flow was not exercised",
    });
    return;
  }
  await page.goto(`${BASE}/dashboard/new`, { waitUntil: "networkidle" });

  // A private origin: the answer that leads somewhere, not a dead end.
  await page.getByLabel("Repository URL").fill("github.com/acme-inc/ledger");
  await page.getByRole("button", { name: "Check origin" }).click();
  const connect = page.getByRole("button", { name: "Connect GitHub" });
  const offered = await connect
    .waitFor({ timeout: 20000 })
    .then(() => true)
    .catch(() => false);
  if (!offered) {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "a private origin did not offer the GitHub connect",
    });
  }
  await shot(
    page,
    "24-probe-private",
    "a private origin, and the one action that helps",
  );
});

await step("connecting GitHub and picking a repository", async () => {
  if (!(await githubConfigured(page))) return;
  // Start disconnected, whatever a previous run left behind, so what
  // this exercises is the *round trip* and not a list that happened to
  // be there already. A pass that silently skips the install is a pass
  // of a different product.
  await page.evaluate(async (base) => {
    const r = await fetch(`${base}/v1/orgs/acme/github/installations`, {
      credentials: "include",
    });
    const { installations = [] } = await r.json();
    for (const i of installations) {
      await fetch(
        `${base}/v1/orgs/acme/github/installations/${i.installation_id}`,
        { method: "DELETE", credentials: "include" },
      );
    }
  }, BASE);
  await page.goto(`${BASE}/dashboard/new`, { waitUntil: "networkidle" });
  await page.getByLabel("Repository URL").fill("github.com/acme-inc/atlas");
  await page.getByRole("button", { name: "Check origin" }).click();

  // Install, which leaves for GitHub and comes back through our
  // callback — the round trip, not a mock of it. Two clicks: the probe's
  // answer opens the picker, and the picker — with nothing connected —
  // is what offers the install.
  await page.getByRole("button", { name: "Connect GitHub" }).click();
  await page
    .getByRole("button", { name: "Install the GitHub app" })
    .click({ timeout: 20000 });
  await page.waitForURL(/connect=/, { timeout: 30000 });
  const outcome = new URL(page.url()).searchParams.get("connect");
  if (outcome !== "ok") {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `coming back from the install said connect=${outcome}`,
    });
    return;
  }
  console.log("  connected, and back on the dashboard");

  await page.goto(`${BASE}/dashboard/new`, { waitUntil: "networkidle" });
  await page
    .getByRole("button", { name: "Pick from your installations" })
    .click();
  // `atlas`, not `widget`: this org already has a repository called
  // widget, and a mirror named after it would be refused as a duplicate
  // — a collision in the fixture reading as a product failure.
  const pick = page.getByRole("button", { name: "acme-inc/atlas" });
  const listed = await pick
    .waitFor({ timeout: 20000 })
    .then(() => true)
    .catch(() => false);
  if (!listed) {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "the installation listed nothing to pick from",
    });
    return;
  }
  // The whole point of the flow: no id is ever shown or typed.
  const body = (await page.locator("body").textContent()) ?? "";
  for (const id of ["4001", "4002"]) {
    if (body.includes(id)) {
      problems.push({
        where: stage,
        kind: "behaviour",
        text: `the picker shows the installation id ${id}`,
      });
    }
  }
  await shot(
    page,
    "25-picker",
    "pick a repository — no installation id anywhere",
  );

  await pick.click();
  // And then the first sync is followed to an end, not left spinning.
  const done = page.getByText(/^git clone |first sync failed/);
  await done.waitFor({ timeout: 60000 }).catch(() => undefined);
  const said = (await done.textContent().catch(() => "")) ?? "";
  if (!said.startsWith("git clone")) {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `the first sync ended on: ${said || "nothing at all"}`,
    });
  } else {
    console.log(`  mirrored: ${said.trim()}`);
  }
  await shot(
    page,
    "26-mirrored",
    "mirrored, followed live, ending on a clone command",
  );

  // And the mirror pushes back. A real `git push` through it lands on
  // the origin first — the bare repository the stack fetches from —
  // and the repository page says where pushes go. Exercising the
  // thing, not the form: the sentence on the page is checked against
  // the origin's own `main`.
  if (said.startsWith("git clone")) {
    const runDir = MAIL_DIR ? path.dirname(MAIL_DIR) : null;
    const bare = runDir
      ? path.join(runDir, "origins", "acme-inc", "atlas.git")
      : null;
    if (!bare || !fs.existsSync(bare)) {
      problems.push({
        where: stage,
        kind: "harness",
        text: `the stack's origin for acme-inc/atlas is not at ${bare} (STRATUM_MAIL_DIR unset, or the stack was built elsewhere)`,
      });
    } else {
      // acme's own admin token, from the stack's bootstrap — not the
      // page cookie. By this stage the browser is signed in as the
      // GitHub user (`ada-dev`), who owns a personal namespace and is
      // no member of `acme`, so a cookie-minted token 404s on acme.
      // The mirror was created with acme's admin session; the git push
      // must use the same identity.
      let tok = null;
      try {
        tok = JSON.parse(
          fs.readFileSync(path.join(runDir, "bootstrap.json"), "utf8"),
        ).admin_token;
      } catch {
        /* reported below when the clone fails */
      }
      const dest = fs.mkdtempSync(path.join(os.tmpdir(), "walk-mirror-push-"));
      const url = new URL("/acme/atlas.git", BASE);
      url.username = "walkthrough";
      url.password = tok ?? "";
      const env = {
        ...process.env,
        GIT_TERMINAL_PROMPT: "0",
        GIT_AUTHOR_NAME: "walkthrough",
        GIT_AUTHOR_EMAIL: "walk@weft.test",
        GIT_COMMITTER_NAME: "walkthrough",
        GIT_COMMITTER_EMAIL: "walk@weft.test",
      };
      const run = (args, cwd) =>
        execFileSync("git", cwd ? ["-C", cwd, ...args] : args, {
          env,
          stdio: "pipe",
          timeout: 60000,
        })
          .toString()
          .trim();
      try {
        // The UI reports the first sync "ready" a beat before the
        // git-serving layout is externally clonable, so retry rather
        // than fail on that window.
        let cloned = false;
        for (let i = 0; i < 5 && !cloned; i++) {
          try {
            run(["clone", "--quiet", url.toString(), dest]);
            cloned = true;
          } catch (e) {
            if (i === 4) throw e;
            fs.rmSync(dest, { recursive: true, force: true });
            await page.waitForTimeout(1500);
          }
        }
        // A unique file per run. The stack's origin bare is not reset
        // between runs, so a fixed filename is already committed from a
        // previous pass and `git commit` answers "nothing to commit" —
        // on stdout, not stderr — which surfaced here as an empty
        // "a push through the mirror failed:" with no reason at all.
        const stamp = Date.now();
        fs.writeFileSync(
          path.join(dest, `through-the-mirror-${stamp}.txt`),
          `pushed through the mirror ${stamp}\n`,
        );
        run(["add", "-A"], dest);
        run(["commit", "-q", "-m", `through the mirror ${stamp}`], dest);
        const pushed = run(["rev-parse", "HEAD"], dest);
        run(["push", "-q", "origin", "main"], dest);
        const atOrigin = run(["rev-parse", "refs/heads/main"], bare);
        if (atOrigin !== pushed) {
          problems.push({
            where: stage,
            kind: "behaviour",
            text: `the origin's main is ${atOrigin.slice(0, 7)} after a push of ${pushed.slice(0, 7)} through the mirror`,
          });
        } else {
          console.log(`  pushed through the mirror: ${pushed.slice(0, 7)} is on the origin`);
        }
      } catch (e) {
        // Both streams: git writes some refusals to stdout (a no-op
        // commit, "Everything up-to-date"), and a message with neither
        // is worse than useless — the reason must always be legible.
        const detail =
          [e.stdout, e.stderr]
            .map((s) => (s ? s.toString() : ""))
            .join(" ")
            .trim() || String(e);
        problems.push({
          where: stage,
          kind: "behaviour",
          text: `a push through the mirror failed: ${detail.replace(/\s+/g, " ").slice(0, 400)}`,
        });
      }
      // Do NOT navigate to /dashboard/repos/atlas here: by this stage
      // the browser is the GitHub user, who is no member of acme and
      // cannot load acme's private mirror, so the tree/branches reads
      // would 404 and be recorded as problems. The "pushes are
      // forwarded" notice is exercised in full by the Playwright suite
      // (mocked API, every push state) and by a live headed check; the
      // product proof here is the real git push above landing on the
      // origin. Shoot the current screen (the first-sync result), which
      // the session can see.
      await shot(page, "27-mirror-push", "a mirror that pushed back through to its origin");
    }
  }

  // Leave the org as it was found. acme's admin token, not the page
  // cookie: the browser is the GitHub user by now and cannot delete an
  // acme repo.
  try {
    const adminTok = JSON.parse(
      fs.readFileSync(path.join(path.dirname(MAIL_DIR), "bootstrap.json"), "utf8"),
    ).admin_token;
    await fetch(`${BASE}/v1/orgs/acme/repos/atlas`, {
      method: "DELETE",
      headers: { authorization: `Bearer ${adminTok}` },
    });
    console.log("  mirror removed again");
  } catch {
    /* the org is torn down with the stack regardless */
  }
});

// Two ways the install round trip really comes back that the happy
// path above never takes. The first real install on weft.sh returned
// from GitHub's *edit installation* page, which sends no `state`, and
// the dashboard showed nothing: the callback said `missing`, and only
// `ok` had a sentence. And the callback used to bind whatever
// installation id it was handed, so long as the state was real — any
// org admin could have connected, and mirrored, another customer's
// installation.
await step(
  "coming back from GitHub without a state, or with somebody else's installation",
  async () => {
    if (!(await githubConfigured(page))) return;
    // Where the fake GitHub lives is whatever the install URL points at.
    const start = async () => {
      const r = await page.evaluate(async (base) => {
        const r = await fetch(`${base}/v1/orgs/acme/github/install`, {
          method: "POST",
          credentials: "include",
          headers: { "content-type": "application/json" },
          body: "{}",
        });
        return await r.json();
      }, BASE);
      return new URL(r.url);
    };

    // An edit to the installation: GitHub returns with the installation
    // id and no state, and the person's own sign-in — they began a
    // connect from acme — is what names the organization.
    const begun = await start();
    await page.goto(
      `${begun.origin}/apps/stratum/installations/update?installation_id=4001`,
    );
    await page.waitForURL(/connect=/, { timeout: 30000 });
    const back = new URL(page.url()).searchParams;
    if (back.get("connect") !== "ok" || back.get("org") !== "acme") {
      problems.push({
        where: stage,
        kind: "behaviour",
        text: `the stateless return said connect=${back.get("connect")} org=${back.get("org")}`,
      });
      return;
    }
    console.log(
      "  a return without a state bound to the org the connect began from",
    );

    // Somebody who does not control installation 4001 arrives with it
    // and a perfectly good state for acme. Nothing binds, and the screen
    // says so in words.
    const again = await start();
    const state = again.searchParams.get("state");
    await page.goto(
      `${again.origin}/apps/stratum/installations/new?installation_id=4001&who=ada&state=${state}`,
    );
    await page.waitForURL(/connect=/, { timeout: 30000 });
    const refused = new URL(page.url()).searchParams.get("connect");
    if (refused !== "notyours") {
      problems.push({
        where: stage,
        kind: "behaviour",
        text: `a stranger's installation came back connect=${refused}`,
      });
      return;
    }
    const said = page
      .getByRole("status")
      .filter({ hasText: /did not confirm that the installation is yours/ });
    const visible = await said
      .waitFor({ timeout: 10000 })
      .then(() => true)
      .catch(() => false);
    if (!visible) {
      problems.push({
        where: stage,
        kind: "behaviour",
        text: "connect=notyours landed with nothing on screen about it",
      });
    }
    await shot(
      page,
      "26b-connect-refused",
      "a refused connect says so, and what to do next",
    );
    // And the installation acme holds is the one it connected itself.
    const held = await page.evaluate(async (base) => {
      const r = await fetch(`${base}/v1/orgs/acme/github/installations`, {
        credentials: "include",
      });
      const { installations = [] } = await r.json();
      return installations.map((i) => i.installation_id);
    }, BASE);
    if (held.join(",") !== "4001") {
      problems.push({
        where: stage,
        kind: "behaviour",
        text: `after the refusals acme holds installations [${held.join(", ")}], wanted [4001]`,
      });
    }
    console.log("  a stranger's installation was refused, in words");
  },
);

// An install that begins on GitHub — the Marketplace listing, the App's
// own page — arrives with no state and, as often as not, nobody signed
// in. It used to dead-end on `connect=missing`. Now the installation is
// parked, the sign-in says why the person is here, and after signing in
// they choose the organization. Driven from a fresh, signed-out context
// because that is the shape it really arrives in; the happy path above
// never leaves the owner's session.
await step("an install begun on GitHub is claimed after signing in", async () => {
  if (!(await githubConfigured(page))) return;
  const begun = new URL(
    (
      await page.evaluate(async (base) => {
        const r = await fetch(`${base}/v1/orgs/acme/github/install`, {
          method: "POST",
          credentials: "include",
          headers: { "content-type": "application/json" },
          body: "{}",
        });
        return await r.json();
      }, BASE)
    ).url,
  );
  // Leave nothing pending from that start: the stateless return below
  // must be parked, not bound to a connect this session began.
  await page.evaluate(async (base) => {
    const r = await fetch(`${base}/v1/orgs/acme/github/installations`, {
      credentials: "include",
    });
    const { installations = [] } = await r.json();
    for (const i of installations) {
      if (i.installation_id === "4002") {
        await fetch(`${base}/v1/orgs/acme/github/installations/4002`, {
          method: "DELETE",
          credentials: "include",
        });
      }
    }
  }, BASE);

  const guest = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const gp = await guest.newPage();
  watch(gp, () => stage);
  try {
    await gp.goto(
      `${begun.origin}/apps/stratum/installations/new?installation_id=4002`,
    );
    await gp.waitForURL(/connect=/, { timeout: 30000 });
    const outcome = new URL(gp.url()).searchParams.get("connect");
    if (outcome !== "claim") {
      problems.push({
        where: stage,
        kind: "behaviour",
        text: `a signed-out install from GitHub's side came back connect=${outcome}, wanted claim`,
      });
      return;
    }
    const why = gp
      .getByRole("status")
      .filter({ hasText: /installation is ready to connect/ });
    if (!(await why.waitFor({ timeout: 10000 }).then(() => true).catch(() => false))) {
      problems.push({
        where: stage,
        kind: "behaviour",
        text: "the sign-in did not say why the person was there",
      });
    }
    await shot(gp, "26c-claim-signin", "sent to sign in, and told why");

    await gp.getByLabel("Email").fill("ada@acme.dev");
    await gp.getByLabel("Password").fill("a long enough password");
    await gp.getByRole("button", { name: "Sign in", exact: true }).click();
    const card = gp.getByRole("form", { name: "Connect the GitHub installation" });
    if (!(await card.waitFor({ timeout: 20000 }).then(() => true).catch(() => false))) {
      problems.push({
        where: stage,
        kind: "behaviour",
        text: "after signing in, no card offered to connect the parked installation",
      });
      return;
    }
    // The card names the account, never the id.
    const text = (await card.textContent()) ?? "";
    if (text.includes("4002")) {
      problems.push({
        where: stage,
        kind: "behaviour",
        text: "the claim card shows the installation id",
      });
    }
    await shot(gp, "26d-claim-card", "signed in: which organization?");
    await card.getByLabel("Organization").selectOption("acme");
    await card.getByRole("button", { name: "Connect" }).click();
    await gp.waitForURL(/connect=ok/, { timeout: 30000 });
    const held = await gp.evaluate(async (base) => {
      const r = await fetch(`${base}/v1/orgs/acme/github/installations`, {
        credentials: "include",
      });
      const { installations = [] } = await r.json();
      return installations.map((i) => i.installation_id);
    }, BASE);
    if (!held.includes("4002")) {
      problems.push({
        where: stage,
        kind: "behaviour",
        text: `after the claim acme holds [${held.join(", ")}], wanted 4002 among them`,
      });
      return;
    }
    console.log("  parked on arrival, connected after sign-in");
    // Leave the org as it was found.
    await gp.evaluate(async (base) => {
      await fetch(`${base}/v1/orgs/acme/github/installations/4002`, {
        method: "DELETE",
        credentials: "include",
      });
    }, BASE);
  } finally {
    await guest.close();
  }
});

// GitHub Actions on the hosted fleet. A workflow that says `runs-on:
// weft` sends GitHub's `workflow_job` webhook here, and the panel under
// Settings → Runners is the only place its fate is written down: on
// GitHub a job we refused looks like a job nobody has picked up.
//
// Two stages. The first reads the installation's card — the one acme
// connected above — and the second delivers one signed `workflow_job`
// the way GitHub does and reads the row it becomes. Neither assumes
// the stack has a GitHub runner configured: a stack without one refuses
// the job with a sentence that names that, and the row has to say so.

/// The secret the stack signs GitHub deliveries with — the same fallback
/// order the server reads them in.
const GITHUB_WEBHOOK_SECRET =
  process.env.STRATUM_GITHUB_WEBHOOK_SECRET ||
  process.env.STRATUM_WEBHOOK_SECRET ||
  null;

/// The Runners settings page, and the panel's own title on it.
async function ghRunnersPanel() {
  await page.goto(`${BASE}/dashboard/settings/runners`, {
    waitUntil: "networkidle",
  });
  await page
    .getByText("GitHub Actions on Weft runners", { exact: true })
    .first()
    .waitFor({ timeout: 15000 });
}

await step("github runners / the card says what to approve", async () => {
  if (!(await githubConfigured(page))) return;
  await ghRunnersPanel();
  // What the server says about the installation, read straight off the
  // route, so the words on the card can be checked against the facts
  // they are supposed to be about.
  const held = await page.evaluate(async (base) => {
    const r = await fetch(`${base}/v1/orgs/acme/github/installations`, {
      credentials: "include",
    });
    return (await r.json()).installations ?? [];
  }, BASE);
  if (held.length !== 1) {
    problems.push({
      where: stage,
      kind: "harness",
      text: `acme holds ${held.length} installation(s) after the connect stages, wanted exactly one`,
    });
    return;
  }
  const [inst] = held;
  const d = inst.detail;
  const ready = page.getByText(/^GitHub Actions on .+ can use Weft runners$/);
  const approve = page.getByRole("link", { name: /^Approve .+ on GitHub$/ });
  const unchecked = page.getByText("Not checked", { exact: true });
  const gone = page.getByText("Uninstalled", { exact: true });
  await Promise.race([
    ready.first().waitFor({ timeout: 15000 }),
    approve.first().waitFor({ timeout: 15000 }),
    unchecked.first().waitFor({ timeout: 15000 }),
    gone.first().waitFor({ timeout: 15000 }),
  ]).catch(() => null);
  const shown = {
    ready: await ready.count(),
    approve: await approve.count(),
    unchecked: await unchecked.count(),
    gone: await gone.count(),
  };
  const words = shown.ready
    ? await ready.first().innerText()
    : shown.approve
      ? await approve.first().innerText()
      : shown.unchecked
        ? "Not checked"
        : shown.gone
          ? "Uninstalled"
          : "(nothing)";
  console.log(
    `  installation ${inst.installation_id}: detail=${JSON.stringify(d)} — card says "${words}"`,
  );

  if (d === null || d === undefined) {
    // The fake GitHub did not answer `GET /app/installations/{id}`, or
    // the server never asked. Either way the card cannot say anything
    // true about permissions, and the pass has not seen the feature.
    problems.push({
      where: stage,
      kind: "harness",
      text: `the server has no detail for installation ${inst.installation_id} — the fake GitHub must answer GET /app/installations/${inst.installation_id} for the card to say anything`,
    });
  } else if (d.gone) {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `GitHub says installation ${inst.installation_id} is gone right after acme connected it`,
    });
  } else {
    // The card must agree with the flags, in words: both held is the
    // ready sentence and nothing to approve; anything missing is a link
    // that names exactly the missing ones, to GitHub's approve page.
    const missing = [];
    if (!d.administration_write) missing.push("Administration: write");
    if (!d.actions_write) missing.push("Actions: write");
    if (missing.length === 0) {
      if (!shown.ready || shown.approve)
        problems.push({
          where: stage,
          kind: "behaviour",
          text: `both permissions are held but the card shows ready=${shown.ready} approve=${shown.approve}`,
        });
    } else {
      const want = `Approve ${missing.join(" and ")} on GitHub`;
      const link = page.getByRole("link", { name: want, exact: true });
      if (!(await link.count()))
        problems.push({
          where: stage,
          kind: "behaviour",
          text: `installation lacks [${missing.join(", ")}] but no link reads "${want}"`,
        });
      else if ((await link.first().getAttribute("href")) !== d.approve_url)
        problems.push({
          where: stage,
          kind: "behaviour",
          text: `the approve link goes to ${await link.first().getAttribute("href")}, not GitHub's ${d.approve_url}`,
        });
      if (shown.ready)
        problems.push({
          where: stage,
          kind: "behaviour",
          text: "the card says the installation can use Weft runners while a permission is missing",
        });
    }
  }
  // The snippet block and the Docker sentence are the same for every
  // installation, and both have to be there before anybody pastes.
  const sizes = page.getByRole("list", { name: "Runner sizes", exact: true });
  const lines = await sizes.getByRole("listitem").allInnerTexts();
  if (lines.length !== 3)
    problems.push({
      where: stage,
      kind: "content",
      text: `the snippet block lists ${lines.length} size(s), wanted 3: ${JSON.stringify(lines)}`,
    });
  for (const label of ["weft", "weft-2x", "weft-4x"])
    if (!lines.some((l) => l.includes(`runs-on: ${label}`)))
      problems.push({
        where: stage,
        kind: "content",
        text: `no snippet line reads "runs-on: ${label}"`,
      });
  console.log(`  sizes: ${lines.map((l) => l.replace(/\s+/g, " ")).join(" | ")}`);
  if (!(await page.getByText(/^docker build and docker run work here without a daemon/).count()))
    problems.push({
      where: stage,
      kind: "content",
      text: "the panel does not say what docker does and does not do on these runners",
    });
  await audit(page, stage);
  await shot(page, "26c-github-runners-card", `the installation's card: ${words}`);
});

await step("github runners / a queued job from GitHub is listed with its refusal", async () => {
  if (!(await githubConfigured(page))) return;
  if (!GITHUB_WEBHOOK_SECRET) {
    problems.push({
      where: stage,
      kind: "harness",
      text: "neither STRATUM_GITHUB_WEBHOOK_SECRET nor STRATUM_WEBHOOK_SECRET is in this shell — eval \"$(scripts/manual-stack.sh env)\" first; without the secret no delivery can be signed",
    });
    return;
  }
  const held = await page.evaluate(async (base) => {
    const r = await fetch(`${base}/v1/orgs/acme/github/installations`, {
      credentials: "include",
    });
    return (await r.json()).installations ?? [];
  }, BASE);
  if (held.length !== 1) {
    problems.push({
      where: stage,
      kind: "harness",
      text: `acme holds ${held.length} installation(s), wanted exactly one to address the delivery to`,
    });
    return;
  }
  const installationId = Number(held[0].installation_id);
  // A job id no earlier run used: GitHub's ids are unique and the intake
  // records "already recorded" for a repeat, which would make a stack a
  // previous run left behind look like a delivery that vanished.
  const jobId = 77_000 + (Date.now() % 1000);
  const runId = 900;
  const body = JSON.stringify({
    action: "queued",
    installation: { id: installationId },
    repository: {
      full_name: "acme/pipeline",
      private: true,
      owner: { login: "acme" },
    },
    workflow_job: {
      id: jobId,
      run_id: runId,
      run_attempt: 1,
      name: "test",
      html_url: `https://github.com/acme/pipeline/actions/runs/${runId}/job/${jobId}`,
      labels: ["self-hosted", "weft"],
      runner_name: null,
      runner_id: null,
      status: "queued",
      started_at: null,
      completed_at: null,
      conclusion: null,
    },
    sender: { login: "somebody" },
  });
  // Signed the way GitHub signs it: HMAC-SHA256 over the exact bytes,
  // hex, with the `sha256=` prefix.
  const sig =
    "sha256=" +
    crypto.createHmac("sha256", GITHUB_WEBHOOK_SECRET).update(body).digest("hex");
  const delivered = await fetch(`${BASE}/webhooks/github`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      "x-github-event": "workflow_job",
      "x-hub-signature-256": sig,
      "x-github-delivery": crypto.randomUUID(),
    },
    body,
  });
  const answer = await delivered.json().catch(() => ({}));
  console.log(`  delivered workflow_job ${jobId}: ${delivered.status} ${JSON.stringify(answer)}`);
  if (delivered.status !== 202 || answer.ignored) {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `a signed workflow_job queued delivery answered ${delivered.status} ${JSON.stringify(answer)}`,
    });
    return;
  }

  // The row, from the route first — so the words on the page can be
  // checked against the record — then on the page.
  let job = null;
  for (let i = 0; i < 30 && !job; i++) {
    const jobs = await page.evaluate(async (base) => {
      const r = await fetch(`${base}/v1/orgs/acme/github-jobs`, {
        credentials: "include",
      });
      return (await r.json()).jobs ?? [];
    }, BASE);
    job = jobs.find((j) => j.github_job_id === jobId) ?? null;
    if (!job) await page.waitForTimeout(1000);
  }
  if (!job) {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `the delivery was accepted as job ${answer.job} but GET /github-jobs never listed github_job_id ${jobId}`,
    });
    return;
  }
  console.log(
    `  recorded: state=${job.state} size=${job.size} ×${job.multiplier} minutes=${job.minutes} refusal=${JSON.stringify(job.refusal)} cancelled_on_github=${job.cancelled_on_github} needs_permission=${job.needs_permission}`,
  );

  await ghRunnersPanel();
  const row = page
    .getByRole("table")
    .first()
    .getByRole("row")
    .filter({ has: page.getByRole("link", { name: "test", exact: true }) })
    .filter({ hasText: "acme/pipeline" })
    .first();
  const listed = await row
    .waitFor({ timeout: 15000 })
    .then(() => true)
    .catch(() => false);
  if (!listed) {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `job ${job.id} is in the list but the panel shows no row for acme/pipeline · test`,
    });
    await shot(page, "26d-github-job-missing", "the delivered job has no row");
    return;
  }
  const cells = await row.getByRole("cell").allInnerTexts();
  const text = cells.map((c) => c.replace(/\s+/g, " ").trim());
  console.log(`  row: ${text.join(" | ")}`);

  // Read the row against the record — the state word, the size, the
  // minutes — rather than only asserting that a row exists. A green
  // pass that had never read the number is how "0 min" could have been
  // "NaN min" for weeks.
  const stateCell = text[3] ?? "";
  if (!stateCell.startsWith(job.state))
    problems.push({
      where: stage,
      kind: "content",
      text: `the row's state reads "${stateCell}" for a job recorded as ${job.state}`,
    });
  if (text[2] !== job.size)
    problems.push({
      where: stage,
      kind: "content",
      text: `the row's size reads "${text[2]}", the record says ${job.size}`,
    });
  if (text[4] !== String(job.minutes))
    problems.push({
      where: stage,
      kind: "content",
      text: `the row's minutes read "${text[4]}", the record says ${job.minutes}`,
    });
  const link = row.getByRole("link", { name: "test", exact: true });
  if ((await link.getAttribute("href")) !== job.html_url)
    problems.push({
      where: stage,
      kind: "content",
      text: `the job links to ${await link.getAttribute("href")}, not GitHub's ${job.html_url}`,
    });

  if (job.state === "refused") {
    // The whole point of the panel: the reason, in the server's own
    // words, and whether GitHub is still holding the job.
    if (!job.refusal || !stateCell.includes(job.refusal))
      problems.push({
        where: stage,
        kind: "content",
        text: `a refused row does not show its refusal ${JSON.stringify(job.refusal)}: "${stateCell}"`,
      });
    const stillQueued = stateCell.includes("still queued on GitHub — cancel it there");
    if (!job.cancelled_on_github && !stillQueued)
      problems.push({
        where: stage,
        kind: "content",
        text: "GitHub is still holding the refused job and the row does not say so",
      });
    if (job.cancelled_on_github && stillQueued)
      problems.push({
        where: stage,
        kind: "content",
        text: "the row tells somebody to cancel a job the server already cancelled on GitHub",
      });
    const approve = row.getByRole("link", { name: "Approve the permission on GitHub", exact: true });
    if (job.needs_permission && !(await approve.count()))
      problems.push({
        where: stage,
        kind: "content",
        text: "the refusal is a missing permission and the row offers no approve link",
      });
    if (job.refusal && job.refusal.includes("no GitHub Actions runner configured"))
      console.log("  this stack has no GitHub Actions runner configured; the row says so, which is the pass for that stack");
  } else if (!["queued", "launching", "running"].includes(job.state)) {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `a job delivered seconds ago is ${job.state}: ${job.error ?? job.refusal ?? "no reason"}`,
    });
  }
  await audit(page, stage);
  await shot(page, "26d-github-job-row", `the delivered job: ${stateCell}`);
});

// Reading code, which until now was possible only by cloning. Every
// read endpoint existed and none was wired to anything.
await step("browsing the code", async () => {
  // One click, not two. The repository's page *is* the file browser,
  // the way GitHub's is; there used to be a "Browse files" button
  // because the repo screen and the browser were two pages.
  await openRepo("widget");
  // The listing, not the word "Loading…": this shot is the one a person
  // reads to judge the file table, and it used to be taken the moment
  // the URL changed, before a single row had arrived.
  await page.locator("table button").first().waitFor({ timeout: 15000 });
  await shot(page, "20-browse-tree", "a directory listing, on a real URL");

  // Into a file. Which file is whatever this repository has, so the
  // walkthrough reads the listing rather than assuming a name — and a
  // root that holds only directories is the ordinary case, not an empty
  // repository, so descend until a blob turns up. Looking only at the
  // root and calling it empty was a bug in this script: `widget`'s root
  // is a single `src/`, and the step reported the product broken.
  let name = "";
  for (let depth = 0; depth < 6 && !name; depth++) {
    // Read the listing only once it has settled, and then click it by
    // name rather than by position. Both halves are load-bearing: a row
    // chosen from the previous directory's listing is the wrong row, and
    // an index into a table that re-renders underneath you resolves to a
    // detached node. `Loading…` clearing is the observable that says the
    // listing on screen is the one this URL asked for.
    await page.waitForLoadState("networkidle");
    await expect(page.getByText("Loading…")).toHaveCount(0);
    const rows = page.locator("table button");
    await rows.first().waitFor({ timeout: 15000 });
    const texts = (await rows.allTextContents()).map((t) => t.trim());
    const target =
      texts.find((t) => !t.endsWith("/")) ?? texts.find((t) => t.endsWith("/"));
    if (!target) break;
    if (target.endsWith("/")) console.log(`  into: ${target}`);
    else name = target;
    const before = page.url();
    await page.getByRole("button", { name: target, exact: true }).click();
    await page.waitForURL((u) => u.href !== before, { timeout: 15000 });
  }
  if (!name) {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "no file anywhere in the tree — nothing to open",
    });
    return;
  }
  console.log(`  opened: ${name}`);

  // Prose opens on its preview, with the source behind a Code tab —
  // and only prose has the tab. A markdown file that opened on source,
  // or a source file that grew a Preview tab, are both defects; so is a
  // preview with no heading when the file starts with one.
  const prose = /\.(md|markdown)$/i.test(name);
  const tabs = page.getByRole("tablist", { name: "File view" });
  const hasTabs = await tabs
    .waitFor({ timeout: prose ? 15000 : 1500 })
    .then(() => true)
    .catch(() => false);
  if (hasTabs !== prose) {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: prose
        ? `${name} is prose and opened without a Preview/Code control`
        : `${name} is source and grew a Preview/Code control`,
    });
  }
  if (hasTabs) {
    const selected = await tabs
      .getByRole("tab", { name: "Preview" })
      .getAttribute("aria-selected");
    if (selected !== "true") {
      problems.push({
        where: stage,
        kind: "behaviour",
        text: `${name} did not open on its preview`,
      });
    }
    await tabs.getByRole("tab", { name: "Code" }).click();
  }

  // Line numbers are the whole point of the view. The code is rendered
  // by the library inside a shadow root (`src/code/surface.tsx`), which
  // Playwright's locators see through; `td` is gone with the table.
  // waitFor, not isVisible: isVisible answers immediately, so it would
  // report "no line numbers" for a view that simply had not painted yet.
  const code = page.locator("diffs-container");
  const numbered = await code
    .getByText("1", { exact: true })
    .first()
    .waitFor({ timeout: 15000 })
    .then(() => true)
    .catch(() => false);
  if (!numbered) {
    problems.push({
      where: stage,
      kind: "content",
      text: "the file view has no line numbers",
    });
  }
  // The repository's tree beside the file, with this file selected.
  const inTree = await page
    .getByRole("treeitem", { name, exact: true })
    .waitFor({ timeout: 15000 })
    .then(() => true)
    .catch(() => false);
  if (!inTree) {
    problems.push({
      where: stage,
      kind: "content",
      text: "the file page has no tree beside it",
    });
  }
  await shot(page, "21-browse-file", "a file with line numbers, linkable");

  // The file page's own history: who last touched *this* file, what each
  // commit did to it, and reading an earlier version. Registering that a
  // history panel exists proves the form; opening an old version and
  // seeing different bytes proves the product.
  const lastBar = await page
    .getByText(/History for this file/)
    .isVisible()
    .catch(() => false);
  if (!lastBar) {
    problems.push({
      where: stage,
      kind: "content",
      text: "the file page has no history panel",
    });
  } else {
    await page.getByText("History for this file").click();
    await expect(page.getByText("Loading…")).toHaveCount(0);
    const versions = page.locator('button[aria-label^="View this file at"]');
    const count = await versions.count();
    if (count === 0) {
      problems.push({
        where: stage,
        kind: "behaviour",
        text: "a committed file has no versions in its history",
      });
    } else {
      await shot(page, "22-file-history", "who last touched this file, and what they did");
      // Read the oldest version listed. The bytes have to differ from
      // the newest, or the switcher is a link that does nothing.
      // Through the shadow root: `innerText` on the host stops at the
      // boundary and answers "" for both versions, which read as "the
      // same content" for two versions that plainly differed on screen.
      const codeText = () =>
        page
          .locator("diffs-container")
          .first()
          .evaluate((e) =>
            [...(e.shadowRoot?.querySelectorAll("[data-line]") ?? [])]
              .map((l) => l.textContent)
              .join("\n"),
          );
      const newest = await codeText();
      await versions.last().click();
      await page
        .getByText(/Viewing this file at/)
        .waitFor({ timeout: 15000 })
        .catch(() =>
          problems.push({
            where: stage,
            kind: "content",
            text: "an older version does not say it is an older version",
          }),
        );
      // The code is highlighted off the main thread and lands a moment
      // after the banner; read it once it has changed, not once the
      // banner has. Reading immediately reported "the same content" for
      // a version that rendered correctly a frame later.
      let oldest = newest;
      if (count > 1) {
        await expect
          .poll(async () => (oldest = await codeText()), {
            timeout: 15000,
          })
          .not.toBe(newest)
          .catch(() => undefined);
      }
      if (count > 1 && oldest === newest) {
        problems.push({
          where: stage,
          kind: "behaviour",
          text: "switching version showed the same content",
        });
      }
      // The rail follows the version: the tree is read at the commit the
      // file is pinned to, and it must land, with this file still the
      // selected row, rather than sit on "Loading…" beside an old file.
      const followed = await page
        .getByRole("treeitem", { name, exact: true })
        .waitFor({ timeout: 15000 })
        .then(() => true)
        .catch(() => false);
      if (!followed) {
        problems.push({
          where: stage,
          kind: "behaviour",
          text: "the file tree did not follow the version switch",
        });
      }
      await shot(page, "23-file-old-version", "an earlier version, and a way back");
      await page.getByRole("button", { name: "Back to latest" }).click();
      await expect(page.getByText(/Viewing this file at/)).toHaveCount(0);
    }
  }

  // The link somebody would send: a fresh context, no history, no
  // clicking — which is the only way to find out whether the URL is
  // really the state.
  const url = page.url();
  const readerCtx = await browser.newContext({
    viewport: { width: 1440, height: 900 },
  });
  const reader = await readerCtx.newPage();
  watch(reader, () => stage);
  await reader.goto(`${BASE}/dashboard/`, { waitUntil: "networkidle" });
  await reader.getByLabel("Email").fill("ada@acme.dev");
  await reader.getByLabel("Password").fill("a long enough password");
  await reader.getByRole("button", { name: "Sign in", exact: true }).click();
  await reader.getByText("Requests today").waitFor({ timeout: 15000 });
  await reader.goto(url, { waitUntil: "networkidle" });
  const arrived = await reader
    .getByText(name, { exact: true })
    .first()
    .isVisible()
    .catch(() => false);
  if (!arrived) {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `following ${url} did not land on ${name}`,
    });
  }
  await audit(reader, stage);
  await readerCtx.close();

  // Back out, so the next step starts where it expects to.
  await page.goto(`${BASE}/dashboard/`, { waitUntil: "networkidle" });
  await page.getByText("Requests today").waitFor({ timeout: 15000 });
});

await step("describing a repository and publishing it", async () => {
  // Both of these live under the repository's Settings tab now. The
  // description is an ordinary saved field there rather than an
  // edit-in-place on a dashboard screen, and publishing sits in the
  // Danger Zone behind typing the repository's name — which is the
  // right ceremony for the one edit whose blast radius is outside the
  // organization.
  await page.goto(`${BASE}/acme/widget/settings`, { waitUntil: "networkidle" });
  const words = `mirrored from GitHub, and searchable — ${Date.now()}`;
  await page.getByLabel("Description").fill(words);
  await page.getByRole("button", { name: "Save", exact: true }).click();
  // Wait for the page to say it saved, not for a timer: polling a
  // request count would pass before the screen had caught up.
  await page.getByRole("status").filter({ hasText: "Saved." }).waitFor({ timeout: 15000 });
  await page.goto(`${BASE}/acme/widget`, { waitUntil: "networkidle" });
  const kept = await page
    .getByText(words, { exact: true })
    .isVisible()
    .catch(() => false);
  if (!kept)
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "the description did not survive being saved",
    });
  await shot(page, "34-repo-described", "a repo that says what it is");

  // Publishing is a separate act, and it is the one that puts this in
  // front of strangers.
  await page.goto(`${BASE}/acme/widget/settings`, { waitUntil: "networkidle" });
  const wasPublic = await page
    .getByRole("button", { name: "Make private" })
    .isVisible()
    .catch(() => false);
  if (!wasPublic) {
    await page.getByRole("button", { name: "Make public" }).click();
    const dialog = page.getByRole("alertdialog");
    // Typing the name is the confirmation, and it is the point: the one
    // ceremony a mis-aimed click cannot satisfy.
    await dialog.getByRole("textbox").fill("widget");
    await dialog
      .getByRole("button", { name: "Make this repository public", exact: true })
      .click();
    await page
      .getByText(/Anyone can read it/)
      .waitFor({ timeout: 15000 });
  }
  await shot(page, "35-repo-public", "published, and said so in words");
});

await step("finding it again from the header", async () => {
  await page.goto(`${BASE}/dashboard/`, { waitUntil: "networkidle" });
  await page.getByLabel("Search repositories").first().fill("widget");
  await page.getByLabel("Search repositories").first().press("Enter");
  await page.waitForURL(/\/dashboard\/search\?q=widget/, { timeout: 15000 });
  await expect(page.getByText("Loading…")).toHaveCount(0);
  const found = await page
    .getByRole("listitem")
    .filter({ hasText: "widget" })
    .first()
    .isVisible()
    .catch(() => false);
  if (!found)
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "searching for a repo that exists found nothing",
    });
  await shot(page, "36-search", "search, across every namespace you are in");

  // A search that matches nothing has to say so. An empty list with no
  // words is indistinguishable from a screen that failed to load.
  await page.getByLabel("Search repositories").last().fill("zzz-no-such-thing");
  await page
    .getByRole("button", { name: "Search", exact: true })
    .last()
    .click();
  // waitFor, not isVisible. `isVisible` answers immediately, so it asks
  // the question one frame after the click and gets "no" from a screen
  // that is still fetching — the same mistake already written down in
  // the file-view step above, made again here. The empty-state message
  // is the observable, so wait for it.
  const said = await page
    .getByText(/Nothing you can see matches/)
    .waitFor({ timeout: 15000 })
    .then(() => true)
    .catch(() => false);
  if (!said)
    problems.push({
      where: stage,
      kind: "content",
      text: "an empty search result says nothing at all",
    });
  await shot(page, "37-search-empty", "nothing found, and it says so");
});

// A repository that publishes a static site, driven the way a person
// would: commit a config and a directory, read the address out of the
// settings panel, then open that address in a browser that has never
// signed in and check that the page is really there.
//
// The last half is the point. Every earlier assertion here could pass
// against a product that stores a hostname and serves nothing, and the
// address is the whole feature. So this stage leaves the dashboard's
// origin entirely and fetches the site as a stranger — which is also
// what proves the separation the design rests on: a context with no
// session, on a different domain, getting the customer's bytes and not
// ours.
await step("sites / a directory published and served", async () => {
  const SITES_DOMAIN = process.env.STRATUM_SITES_DOMAIN;
  if (!SITES_DOMAIN) {
    // A problem, not a skip. The same rule as a missing SSH URL: a pass
    // against a stack that cannot host sites is a walkthrough of a
    // different product, and reporting it as fine is how the SSH clone
    // row went untested for weeks.
    problems.push({
      where: stage,
      kind: "harness",
      text:
        "STRATUM_SITES_DOMAIN is not set, so no site can be served — " +
        'eval "$(scripts/manual-stack.sh env)" before running this',
    });
    return;
  }

  const REPO = "pages-demo";
  const made = await page.evaluate(async (repo) => {
    const post = async (path, body) => {
      const r = await fetch(path, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      });
      return r.status;
    };
    const out = {};
    out.repo = await post("/v1/orgs/acme/repos", {
      name: repo,
      public: true,
      description: "a static site",
    });
    out.commit = await post(`/v1/orgs/acme/repos/${repo}/commits`, {
      message: "publish the handbook",
      operations: [
        { op: "put", path: ".weft/site.yml", content: "publish: dist\nnot-found: 404.html\n" },
        {
          op: "put",
          path: "dist/index.html",
          content: [
            "<!doctype html>",
            '<html lang="en"><head><meta charset="utf-8">',
            '<meta name="viewport" content="width=device-width, initial-scale=1">',
            "<title>Acme handbook</title>",
            '<link rel="stylesheet" href="/style.css">',
            "</head><body>",
            "<h1>Acme handbook</h1>",
            "<p>Published from a directory we already had.</p>",
            '<p>Read <a href="/guide">the guide</a>.</p>',
            "</body></html>",
          ].join("\n"),
        },
        {
          op: "put",
          path: "dist/style.css",
          content:
            "body{font:16px/1.5 system-ui,sans-serif;margin:3rem auto;max-width:34rem;padding:0 1rem}",
        },
        {
          op: "put",
          path: "dist/guide/index.html",
          content:
            '<!doctype html><html lang="en"><head><meta charset="utf-8">' +
            '<title>The guide</title><link rel="stylesheet" href="/style.css">' +
            "</head><body><h1>The guide</h1></body></html>",
        },
        {
          op: "put",
          path: "dist/404.html",
          content:
            '<!doctype html><html lang="en"><head><meta charset="utf-8">' +
            '<title>Not found</title><link rel="stylesheet" href="/style.css">' +
            "</head><body><h1>Not a page we have</h1></body></html>",
        },
        // Outside the published directory. Nothing may reach it.
        { op: "put", path: "NOTES.md", content: "internal only\n" },
      ],
    });
    return out;
  }, REPO);
  if (made.repo !== 201 || made.commit !== 201) {
    problems.push({
      where: stage,
      kind: "harness",
      text: `could not set the stage: ${JSON.stringify(made)}`,
    });
    return;
  }

  // Publishing is a queued job, so wait on the thing the next assertion
  // needs — the address the API reports — rather than on a sleep.
  let status = null;
  for (let i = 0; i < 60; i++) {
    status = await page.evaluate(async (repo) => {
      const r = await fetch(`/v1/orgs/acme/repos/${repo}/site`);
      return r.ok ? await r.json() : { error: r.status };
    }, REPO);
    if (status && status.url) break;
    await page.waitForTimeout(500);
  }
  if (!status || !status.url) {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `no site address after 30s: ${JSON.stringify(status)}`,
    });
    return;
  }
  console.log(`  address: ${status.url}`);

  // The settings panel is where a person actually finds this.
  // The **forge** mount, not the dashboard one. The dashboard's repo
  // page has two tabs and no Settings tab at all; `/{owner}/{repo}` is
  // where the settings panel lives. Reaching for the dashboard's
  // furniture here is the same mistake the branch-policy stage above
  // records having made.
  await page.goto(`${BASE}/acme/${REPO}/settings`, {
    waitUntil: "networkidle",
  });
  await page.getByRole("heading", { name: "Site", exact: true }).waitFor({ timeout: 15000 });
  await shot(page, "site-01-settings", "the Site panel, with the published address");

  const host = status.host;
  if (host && !(await page.getByText(host, { exact: false }).count())) {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `the settings panel does not show the address ${host} a visitor needs`,
    });
  }
  // The footgun, stated where it cannot be missed: publishing makes a
  // directory public whatever the repository's visibility.
  //
  // Matched on the sentence rather than on the word "public", which
  // appears on this page anyway as the repository's own visibility —
  // a check that cannot fail is worse than no check, and `getByText`
  // matches by substring.
  const warned = await page
    .getByText("Anyone with the address can read every", { exact: false })
    .count();
  if (!warned) {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "the panel does not warn that a published site is public even from a private repository",
    });
  }

  // Now as a stranger, on the site's own domain, with no session at all.
  const siteBase = status.url.replace(/^https:/, "http:").replace(/\/$/, "");
  const port = new URL(BASE).port;
  const visitUrl = port
    ? siteBase.replace(`${SITES_DOMAIN}`, `${SITES_DOMAIN}:${port}`)
    : siteBase;
  const visitorCtx = await browser.newContext({
    viewport: { width: 1440, height: 900 },
  });
  const visitor = await visitorCtx.newPage();
  watch(visitor, () => stage);
  try {
    await visitor.goto(`${visitUrl}/`, { waitUntil: "networkidle" });
    await visitor
      .getByRole("heading", { name: "Acme handbook" })
      .waitFor({ timeout: 15000 });
    await shot(visitor, "site-02-served", "the published site, as a visitor sees it");
    await audit(visitor, stage);

    // A directory without a trailing slash redirects, so relative links
    // inside the page resolve against it rather than its parent.
    await visitor.goto(`${visitUrl}/guide`, { waitUntil: "networkidle" });
    if (!visitor.url().endsWith("/guide/")) {
      problems.push({
        where: stage,
        kind: "behaviour",
        text: `/guide did not redirect to /guide/ — landed on ${visitor.url()}`,
      });
    }

    // The committed 404 page, with a 404 status. A missing page that
    // reports success is one search engines index.
    const missing = await visitor.goto(`${visitUrl}/nowhere-at-all`, {
      waitUntil: "networkidle",
    });
    if (missing && missing.status() !== 404) {
      problems.push({
        where: stage,
        kind: "behaviour",
        text: `a missing page answered ${missing.status()}, not 404`,
      });
    }
    if (!(await visitor.getByText("Not a page we have").count())) {
      problems.push({
        where: stage,
        kind: "behaviour",
        text: "the repository's own 404.html was not served for a missing page",
      });
    }

    // Nothing above the published directory is reachable.
    const leak = await visitor.goto(`${visitUrl}/NOTES.md`, {
      waitUntil: "networkidle",
    });
    const leaked = await visitor.content();
    if (leaked.includes("internal only")) {
      problems.push({
        where: stage,
        kind: "behaviour",
        text: "a file outside the published directory was served",
      });
    } else {
      benign.push({
        where: stage,
        kind: "expected-refusal",
        text: `a file outside the published directory answered ${leak && leak.status()}`,
      });
    }

    // And the product is not on this domain. A dispatch bug does not
    // 404 here — it answers with our dashboard under the customer's
    // address, which is why this looks at the bytes.
    await visitor.goto(`${visitUrl}/dashboard/`, { waitUntil: "networkidle" });
    const onSiteDomain = (await visitor.content()).toLowerCase();
    if (onSiteDomain.includes("sign in") || onSiteDomain.includes("requests today")) {
      problems.push({
        where: stage,
        kind: "behaviour",
        text: "the dashboard rendered on the sites domain — dispatch fell through to the router",
      });
    }
  } finally {
    await visitorCtx.close();
  }

  // Back where the next stage expects to be.
  await page.goto(`${BASE}/dashboard/`, { waitUntil: "networkidle" });
  await page.getByText("Requests today").waitFor({ timeout: 15000 });
});

await step("the public discovery page, signed out", async () => {
  // A fresh context with no cookie: this is the page a stranger sees,
  // and the only way to prove the anonymous path is what it claims is
  // to arrive without a session. Signing out of `page` would not do —
  // the point is that nothing was ever sent.
  const strangerCtx = await browser.newContext({
    viewport: { width: 1440, height: 900 },
  });
  const stranger = await strangerCtx.newPage();
  watch(stranger, () => stage);
  await stranger.goto(`${BASE}/discover`, { waitUntil: "networkidle" });
  const listed = await stranger
    .getByText("widget", { exact: false })
    .first()
    .isVisible()
    .catch(() => false);
  if (!listed)
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "a public repo is not on the public discovery page",
    });
  // ...and the private one is not, which is the assertion that matters.
  const leaked = await stranger
    .getByText("ledger", { exact: false })
    .first()
    .isVisible()
    .catch(() => false);
  if (leaked)
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "a private repo is listed on the public discovery page",
    });
  await shot(stranger, "38-discover", "public repositories, no account");
  await audit(stranger, stage);
  await strangerCtx.close();
});

await step("the public repository page, signed out", async () => {
  // The claim the whole open-source motion rests on: somebody with no
  // account follows a link and reads the code. A fresh context, because
  // signing out of `page` would leave this proving something weaker —
  // that a session can be discarded, rather than that none was needed.
  const strangerCtx = await browser.newContext({
    viewport: { width: 1440, height: 900 },
  });
  const stranger = await strangerCtx.newPage();
  watch(stranger, () => stage);
  await stranger.goto(`${BASE}/acme/widget`, { waitUntil: "networkidle" });

  const readable = await stranger
    .getByRole("link", { name: "widget", exact: true })
    .first()
    .isVisible()
    .catch(() => false);
  if (!readable)
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "a public repository does not open for somebody with no account",
    });
  // The file table, with its last-commit column filled: this is the
  // page a stranger judges the product by, and the shot named for it
  // used to be taken after the stage had moved on to the *private*
  // repository, so "a public repo, no account" was a picture of a 404.
  await stranger
    .locator("table button")
    .first()
    .waitFor({ timeout: 15000 })
    .catch(() => {});
  await shot(stranger, "39-public-repo", "a public repo, no account");

  // No dashboard chrome. The rail's trigger exists on every signed-in
  // page and must exist on none of these: a forge page that quietly
  // rendered inside the admin shell would look almost right, and be
  // wrong in the way nobody notices until a stranger says they were
  // asked to sign in to read open source.
  if (await stranger.getByRole("button", { name: "Toggle sidebar" }).count())
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "the public repo page is wearing the signed-in dashboard's sidebar",
    });

  // The identity row, which is the four seconds a migrating project
  // judges us in (FORGE-UX §1). Three things a screenshot alone would
  // not have caught, and one of them shipped wrong for months.
  //
  // Visibility, said out loud. A public repository that does not say it
  // is public leaves the maintainer who just published it with no way
  // to confirm from the page that they did.
  if (
    !(await stranger
      .getByText("Public", { exact: true })
      .first()
      .isVisible()
      .catch(() => false))
  )
    problems.push({
      where: stage,
      kind: "content",
      text: "the repository page never says whether it is public or private",
    });

  // All three controls, to a stranger, each carrying its count. These
  // are the honest signals somebody with no account has about whether
  // anybody is here, and Watch used to render nothing at all for them —
  // which also meant the row changed width once the auth probe
  // answered, on every repository page.
  // All three unanchored, on purpose. `aria-label` *replaces* the
  // accessible name, so a stranger's controls are named "Sign in to
  // watch", "Sign in to fork" and "Sign in to star" rather than
  // "Watch 12". An anchored `/^star/i` matched only the signed-in
  // wording and would have reported this stage's own subject —
  // "a stranger is offered no star control" — as a product defect the
  // moment Star was made to explain itself like its two siblings.
  for (const [what, name] of [
    ["watch", /watch/i],
    ["fork", /fork/i],
    ["star", /star/i],
  ]) {
    if (!(await stranger.getByRole("button", { name }).count()))
      problems.push({
        where: stage,
        kind: "behaviour",
        text: `a stranger is offered no ${what} control on a public repository`,
      });
  }

  // And they are one row, not three buttons that happen to be adjacent.
  // Star used to hang an origin-stars caption below itself, which made
  // it taller than its two siblings and pushed the row's baseline — the
  // kind of defect that reads as "this page is slightly wrong" without
  // anybody being able to say why.
  const tops = await stranger.evaluate(() =>
    [...document.querySelectorAll("header button")]
      .map((b) => b.getBoundingClientRect())
      .filter((r) => r.width > 0)
      .map((r) => Math.round(r.top)),
  );
  if (tops.length > 1 && Math.max(...tops) - Math.min(...tops) > 1)
    problems.push({
      where: stage,
      kind: "layout",
      text: `the identity row's controls do not share a baseline: tops ${tops.join(", ")}`,
    });

  // The About rail, which is the OSS calling card and the densest
  // piece of community signalling on the page (FORGE-UX §1).
  //
  // All six health rows, always — present or absent. This is the one
  // difference from GitHub worth checking with a person's eye, because
  // GitHub silently omits a row when the file does not exist, which
  // makes an incomplete project look identical to a complete one. A
  // maintainer should see the gap.
  for (const row of [
    "Readme",
    "License",
    "Code of conduct",
    "Contributing",
    "Security policy",
    "Activity",
  ]) {
    const seen = await stranger
      .getByRole("listitem")
      .filter({ hasText: row })
      .count();
    if (seen !== 1)
      problems.push({
        where: stage,
        kind: "content",
        text: `the About rail has ${seen} "${row}" rows, expected exactly 1`,
      });
  }

  // And an absent file says "None" in words. A grey glyph beside a
  // brand-coloured one is a state readable only by colour, which
  // DESIGN.md forbids and which a red-green colourblind reader gets
  // wrong every time.
  const absentSaysSo = await stranger
    .getByText("None", { exact: true })
    .first()
    .isVisible()
    .catch(() => false);
  if (!absentSaysSo)
    problems.push({
      where: stage,
      kind: "content",
      text: "no health row says 'None' — absence is being shown by colour alone, or every file exists in the fixture and this check is vacuous",
    });

  // The counts block. Three numbers a stranger uses to decide whether
  // anybody is here, each with its own word beside it.
  //
  // Singular **and** plural, because the rail says "1 star" and not
  // "1 stars" — English, and `about.tsx` says so on purpose. This check
  // demanded the plural, so the moment anything in the pass starred the
  // repository it reported a defect against correct behaviour. A
  // walkthrough that cries wolf on the product being right is worse than
  // one that stays quiet: the next reader learns to skim its output.
  //
  // And since the pair is what is being looked for, the agreement is
  // worth asserting too — it is the thing the rail claims and nothing
  // else here checks.
  for (const [singular, plural] of [
    ["star", "stars"],
    ["watching", "watching"],
    ["fork", "forks"],
  ]) {
    const row = stranger
      .getByRole("listitem")
      .filter({ hasText: new RegExp(`\\b\\d+\\s+(${singular}|${plural})\\b`) })
      .first();
    if (!(await row.isVisible().catch(() => false))) {
      problems.push({
        where: stage,
        kind: "content",
        text: `the About rail never says "${plural}"`,
      });
      continue;
    }
    const text = ((await row.textContent()) ?? "").trim();
    // `\b` on the tail, or `fork` matches the front of `forks` and the
    // check reports "0 fork" against a rail that said "0 forks".
    const m = text.match(
      new RegExp(`(\\d+)\\s+(${singular}|${plural})\\b`),
    );
    if (m && singular !== plural) {
      const want = m[1] === "1" ? singular : plural;
      if (m[2] !== want)
        problems.push({
          where: stage,
          kind: "content",
          text: `the About rail says "${m[1]} ${m[2]}", which is not English`,
        });
    }
  }

  // A README that exists is a link, not the word "None".
  //
  // This is here because it shipped wrong and no server-side test could
  // see it: `/meta` grew a `readme` field without its ETag moving, so
  // every returning visitor was answered 304 and kept a cached body
  // with no README in it. What a person saw was "Readme — None" on a
  // page rendering the README directly underneath. Only a browser
  // holding a real cache could produce it, which is exactly what this
  // pass is.
  const readmeInTree = await stranger
    .getByText("README.md", { exact: false })
    .first()
    .isVisible()
    .catch(() => false);
  if (readmeInTree) {
    const readmeLinked = await stranger
      .getByRole("link", { name: "Readme", exact: true })
      .isVisible()
      .catch(() => false);
    if (!readmeLinked)
      problems.push({
        where: stage,
        kind: "content",
        text: "the file listing shows a README and the About rail says it has none — a stale cached /meta body, or the readme field is not reaching the rail",
      });
  }

  // The repository is named once. The masthead says it; the file
  // listing's breadcrumb root said it again, twelve pixels lower, in
  // the same weight, as a button that navigated to the page it was
  // already on.
  const namedTwice = await stranger
    .getByRole("button", { name: "widget", exact: true })
    .count();
  if (namedTwice)
    problems.push({
      where: stage,
      kind: "layout",
      text: "the repository name is rendered twice at the repository root",
    });

  // The private repository must not be readable at the same shape of
  // address. This is the assertion that matters: the page is new, and
  // every new page is a new way to leak that something exists.
  await stranger.goto(`${BASE}/acme/ledger`, { waitUntil: "networkidle" });
  const leaked = await stranger
    .getByText("Recent commits", { exact: false })
    .first()
    .isVisible()
    .catch(() => false);
  if (leaked)
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "a private repository rendered its contents to a stranger",
    });

  await shot(
    stranger,
    "39a-private-repo-hidden",
    "a private repo, to a stranger: not there at all",
  );
  await audit(stranger, stage);
  await strangerCtx.close();
});

await step("the checks tab, signed out", async () => {
  // CI, which we do not run. The whole claim of this tab is that a
  // project's verdicts show up here whoever produced them, and the
  // failure mode it exists to avoid is an empty list — which reads as
  // "this project has no CI" when the truth may be that an App
  // installation cannot read Actions, or that nobody has wired their
  // CI up yet. Those are three different states and a person has to be
  // able to tell them apart.
  const ctx = await browser.newContext({
    viewport: { width: 1440, height: 900 },
  });
  const stranger = await ctx.newPage();
  watch(stranger, () => stage);
  await stranger.goto(`${BASE}/acme/widget/checks`, {
    waitUntil: "networkidle",
  });

  // The tab is reachable and is the one lit.
  const strip = stranger.getByRole("link", { name: "Checks", exact: true });
  if (!(await strip.count()))
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "a public repository has no Checks tab",
    });

  // Whatever this repository's state is, the page says something a
  // person can act on. An empty panel with no sentence in it is the
  // defect — it is indistinguishable from a broken fetch.
  const body = (await stranger.locator("main").innerText()).trim();
  if (body.replace(/\s+/g, " ").length < 40)
    problems.push({
      where: stage,
      kind: "content",
      text: `the Checks tab renders almost nothing: ${JSON.stringify(body.slice(0, 120))}`,
    });

  // Never the bare word "Actions" as a heading or a tab. We do not run
  // anybody's workflows, and borrowing the name implies we do — this is
  // FORGE-UX §7's rule, and it is the kind of thing that creeps back in
  // one label at a time.
  if (await stranger.getByRole("link", { name: "Actions", exact: true }).count())
    problems.push({
      where: stage,
      kind: "content",
      text: "a tab is labelled Actions — we do not run workflows (FORGE-UX §7)",
    });

  // The address somebody arriving from GitHub types has to land on the
  // real tab rather than 404ing to prove a naming point.
  await stranger.goto(`${BASE}/acme/widget/actions`, {
    waitUntil: "networkidle",
  });
  if (!/\/checks$/.test(new URL(stranger.url()).pathname))
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `/{owner}/{repo}/actions did not redirect to the Checks tab: ${stranger.url()}`,
    });

  await shot(stranger, "39b-checks", "CI verdicts we did not produce");
  await audit(stranger, stage);
  await ctx.close();
});

await step("the public profile page, signed out", async () => {
  // The namespace page had never been in this pass at all, which is how
  // it went a long time rendering an avatar, a handle and a grid while
  // the identity substrate behind it — display name, bio, pronouns,
  // company, location, links, pins — was already served and read by
  // nothing.
  //
  // What this stage can prove today is bounded, and worth saying out
  // loud rather than leaving for somebody to discover: `manual-stack.sh`
  // seeds the `acme` org and no personal handle, sets no profile fields
  // and pins nothing, so the rail here renders its fallback — the
  // handle as its own heading and no metadata list. That is the correct
  // rendering of an empty profile and this stage holds it, but it is
  // not a test of a filled-in one. See the note in `profile.spec.ts`;
  // filling it needs a seeded personal namespace.
  //
  // The layout half is not bounded, and it is the half that matters
  // most here: the rail is a new fixed-width flex column beside a grid,
  // which is the exact shape `audit()`'s overflow check exists for.
  const strangerCtx = await browser.newContext({
    viewport: { width: 1440, height: 900 },
  });
  const stranger = await strangerCtx.newPage();
  watch(stranger, () => stage);
  await stranger.goto(`${BASE}/acme`, { waitUntil: "networkidle" });

  // With no display name set, the handle is the heading. A page that
  // fell back to an empty string would still "have a heading", so this
  // asks for the text.
  const named = await stranger
    .getByRole("heading", { name: "acme", level: 1 })
    .isVisible()
    .catch(() => false);
  if (!named)
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "a namespace page does not name the namespace",
    });

  // The grid is the substance of the page for a stranger, and it is
  // fed by search rather than the protected listing precisely so that
  // a signed-out visitor gets it.
  const listed = await stranger
    .getByRole("link", { name: "widget", exact: false })
    .first()
    .isVisible()
    .catch(() => false);
  if (!listed)
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "a public repo is missing from its own namespace page",
    });

  // Every new page is a new way to leak that something exists, and this
  // one asks the server three questions instead of one.
  const leaked = await stranger
    .getByText("ledger", { exact: false })
    .first()
    .isVisible()
    .catch(() => false);
  if (leaked)
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "a private repo is listed on a namespace page to a stranger",
    });

  // An empty profile must not grow furniture: no "Pinned" over nothing,
  // and no metadata rows with nothing in them. Omit rather than zero.
  if (await stranger.getByRole("heading", { name: "Pinned" }).count())
    problems.push({
      where: stage,
      kind: "content",
      text: "a 'Pinned' heading over a profile with no pins",
    });

  await shot(stranger, "39b-public-profile", "a namespace, no account");
  await audit(stranger, stage);
  await strangerCtx.close();
});

await step("signing in from a public page returns to it", async () => {
  // An invitation to sign in must not be a one-way door. Sending
  // somebody to a login form and then dropping them on a dashboard they
  // never asked for loses the thing they were reading, and the thing
  // they were reading is why they came.
  const strangerCtx = await browser.newContext({
    viewport: { width: 1440, height: 900 },
  });
  const stranger = await strangerCtx.newPage();
  watch(stranger, () => stage);
  await stranger.goto(`${BASE}/acme/widget`, { waitUntil: "networkidle" });
  const href = await stranger
    .getByRole("link", { name: "Sign in" })
    .first()
    .getAttribute("href")
    .catch(() => null);
  if (href !== `/login?next=${encodeURIComponent("/acme/widget")}`)
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `sign-in from a public page does not come back to it (href ${href})`,
    });
  await audit(stranger, stage);
  await strangerCtx.close();
});

await step("packages / the registry is off until an admin switches it on", async () => {
  // The whole loop, not the form: switch npm on, publish through npm's
  // own wire protocol, and see the version — with the licence and the
  // size — on the screen. Seeing a row appear in a table proves the row
  // was written; it does not prove anybody could install it.
  await page.goto(`${BASE}/dashboard/settings/packages`, {
    waitUntil: "networkidle",
  });
  // Panel titles are CardTitle divs, not headings — match the text.
  await page
    .getByText("Ecosystems", { exact: true })
    .first()
    .waitFor({ timeout: 15000 });

  const before = (await page.innerText("body")).replace(/\s+/g, " ");
  for (const want of ["npm", "Maven", "PyPI", "Cargo", "OCI"])
    if (!before.includes(want))
      problems.push({
        where: stage,
        kind: "content",
        text: `the Packages settings page does not list ${want}`,
      });
  // Nothing is on, so there is nothing to point a client at yet. This is
  // the assertion that would catch a snippet rendered for a registry
  // that answers 404 — somebody would paste it and blame their token.
  if (before.includes("_authToken"))
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "the .npmrc snippet is shown before any ecosystem is enabled",
    });
  await shot(page, "50a-packages-off", "five ecosystems, all off, no snippet");

  // Switch npm on the way a person does: a Radix select, not a <select>.
  await page.getByRole("combobox", { name: "npm registry mode" }).click();
  await page.getByRole("option", { name: "Private", exact: true }).click();
  await page
    .getByText("Pointing npm at it", { exact: true })
    .first()
    .waitFor({ timeout: 15000 });

  const snippet = (await page.innerText("body")).replace(/\s+/g, " ");
  if (!snippet.includes("@acme:registry="))
    problems.push({
      where: stage,
      kind: "content",
      text: "npm is enabled but the page shows no .npmrc snippet to configure against",
    });
  await shot(page, "50b-packages-on", "npm private, with the snippet a developer pastes");
});

await step("packages / a published version shows the commit that built it", async () => {
  // Published through npm's own publish document, from the browser, so
  // the bytes go through the same door `npm publish` uses.
  const published = await page.evaluate(async () => {
    const b64 = (s) => btoa(s);
    const tar = "a tarball, near enough";
    const doc = {
      _id: "@acme/widget-lib",
      name: "@acme/widget-lib",
      "dist-tags": { latest: "1.2.3" },
      versions: {
        "1.2.3": {
          name: "@acme/widget-lib",
          version: "1.2.3",
          license: "MIT",
          dependencies: { "left-pad": "^1.0.0" },
        },
      },
      _attachments: {
        "widget-lib-1.2.3.tgz": {
          content_type: "application/octet-stream",
          data: b64(tar),
          length: tar.length,
        },
      },
    };
    const r = await fetch("/v1/registry/npm/acme/@acme%2fwidget-lib", {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(doc),
    });
    return { status: r.status, body: await r.text() };
  });
  if (published.status !== 201) {
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `publishing to the registry answered ${published.status}: ${published.body.slice(0, 200)}`,
    });
    return;
  }

  // And it resolves, the way a resolver would read it.
  const resolved = await page.evaluate(async () => {
    const r = await fetch("/v1/registry/npm/acme/@acme%2fwidget-lib");
    if (r.status !== 200) return { status: r.status };
    const doc = await r.json();
    const v = doc.versions?.["1.2.3"] ?? {};
    return {
      status: r.status,
      tarball: v.dist?.tarball ?? null,
      integrity: v.dist?.integrity ?? null,
      dep: v.dependencies?.["left-pad"] ?? null,
      tag: doc["dist-tags"]?.latest ?? null,
    };
  });
  if (resolved.status !== 200)
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `the packument answered ${resolved.status}`,
    });
  if (resolved.dep !== "^1.0.0")
    problems.push({
      where: stage,
      kind: "behaviour",
      text: "the packument carries no dependencies — every install of this package would resolve none of them",
    });
  if (!String(resolved.integrity ?? "").startsWith("sha512-"))
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `the packument advertises no integrity for npm to check (${resolved.integrity})`,
    });
  if (resolved.tag !== "1.2.3")
    problems.push({
      where: stage,
      kind: "behaviour",
      text: `dist-tags.latest is ${resolved.tag}, not the version just published`,
    });

  // The screen, which is the part a person meets.
  await page.reload({ waitUntil: "networkidle" });
  await page
    .getByText("@acme/widget-lib", { exact: true })
    .first()
    .waitFor({ timeout: 15000 });
  await page.getByRole("button", { name: "Versions" }).first().click();
  // The button fetches the detail and *then* renders the panel, so read
  // after the panel exists rather than after the click. Waiting on the
  // "Licence" column header rather than on the values below keeps the
  // assertions that follow able to fail: waiting for "1.2.3" would make
  // the check for "1.2.3" pass by construction.
  await page
    .getByRole("columnheader", { name: "Licence" })
    .first()
    .waitFor({ timeout: 15000 });
  const shown = (await page.innerText("body")).replace(/\s+/g, " ");
  // Read the numbers, not just that the panel rendered.
  for (const want of ["1.2.3", "MIT"])
    if (!shown.includes(want))
      problems.push({
        where: stage,
        kind: "content",
        text: `the versions panel does not show "${want}"`,
      });
  if (!shown.includes("published by hand"))
    problems.push({
      where: stage,
      kind: "content",
      text: "a version published with a token shows no provenance at all — it should say so rather than leaving the column blank",
    });
  // Frame the panel this shot is named for. It renders below the fold,
  // so without the scroll the artifact a reviewer opens shows the
  // ecosystem toggles and stops just above the version rows — a
  // screenshot that does not contain the thing its caption promises is
  // the same failure as a stage that asserts nothing.
  await page
    .getByRole("columnheader", { name: "Licence" })
    .first()
    .scrollIntoViewIfNeeded();
  await shot(page, "50c-packages-version", "the published version, its licence and where it came from");
});

// Outside the last step, not inside it: `step` audits the page after the
// body returns, so closing the browser in there tore the page out from
// under the audit. Every run that reached the end died on
// "Target page, context or browser has been closed" — and the runs that
// did not reach it were the ones that bailed out early, which is why
// this looked like it worked.
await browser.close();
fs.writeFileSync(
  `${OUT}/report.json`,
  JSON.stringify({ problems, benign, shots, failedStages }, null, 2),
);

console.log(
  `\n=== ${problems.length} problem(s), ${benign.length} expected refusal(s)/artifact(s), ${shots.length} screenshots -> ${OUT}`,
);
if (failedStages.length)
  // Said before the counts and again in the exit code: a run with a
  // failed stage did not look at the whole product, so "0 problems"
  // could never have been said about it honestly anyway.
  console.log(
    `!!! ${failedStages.length} stage(s) threw and everything after them ran against` +
      ` whatever state that left: ${failedStages.join(", ")}`,
  );
const byKind = {};
for (const p of problems) byKind[p.kind] = (byKind[p.kind] ?? 0) + 1;
console.log(byKind);
for (const p of problems.slice(0, 40))
  console.log(` [${p.where}] ${p.kind}: ${p.text}`);
