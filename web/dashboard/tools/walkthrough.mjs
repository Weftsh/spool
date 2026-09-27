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
  // A signed-out context arriving from GitHub's install page: the boot
  // probe asks "am I signed in?" and is told no, which is the point.
  "an install begun on GitHub is claimed after signing in": [401],
  "settings / tokens": [401], // the token we just revoked, checked again
  "repo view": [404], // a viewer asking for the access map
  // Browsing starts from the repo screen, which asks for the access map
  // again — and a viewer is refused it again, by the same masking 404.
  // The 401 is the boot probe in the fresh context this step opens to
  // follow the file link the way somebody who was sent it would.
  "browsing the code": [404, 401],
  // the point of the stage: a direct commit to protected trunk is 403
  "changes / protect trunk": [403],
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
  // refusal came back is still fading from 60% opacity to full when the
  // frame is taken, and a shot once showed every button on a form
  // washed out — which reads as "all disabled" to anyone looking at the
  // picture later. Bounded: an animation that never ends must
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

await step("settings / members", async () => {
  await page.getByRole("link", { name: "Members" }).click();
  await page.getByText("Dev Person").waitFor();
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
  // Behind the Code button, on the repository's own page — it used to
  // be on a dashboard screen at no address of its own.
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
  // browsing, the Checks tab. A gate that reports
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
  // forge deliberately has no org-wide changeset list — it lives on the
  // dashboard — and `forgeChangesetLinks` sets `list: null` rather than
  // offer a link to a 404. There is therefore no sidebar and no way back to a
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
// Workflows: the `.weft/*.yml` files the forge runs ITSELF, on the
// organisation's own runners.
//
// The `ci /` stages above prove the loop when somebody else owns the
// build. These prove the other one: a workflow file in the repository,
// a job taken by a runner the organisation registered, and a verdict
// that arrives on the same Checks tab without anybody posting it. The
// jobs themselves run in the self-hosted stages below, on the REAL
// `weft-runner` binary; the first stages here need no runner at all,
// because a file that does not parse is refused before anything is
// dispatched.
//
// Nothing here is mocked in the browser. The push is the real git CLI
// over HTTP with a minted token. `scripts/manual-stack.sh` seeds
// `acme/${RUNNER_WF_REPO}` and exports RUNNER_WF_REPO.
// ---------------------------------------------------------------------
const WF_REPO = process.env.RUNNER_WF_REPO ?? 'builds';
const WF_BRANCH = `wf-review-${Date.now()}`;
const WF_DIR = fs.mkdtempSync(path.join(os.tmpdir(), 'walk-wf-'));
let wfLive = false;
let wfPushToken = null;

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

await step('workflows / the workflow repository can be pushed to', async () => {
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
  wfPushToken = minted.body.token;
  // One checkout for every workflow stage below, each of which branches
  // off `WF_BRANCH` so that no stage builds on another's file.
  const url = new URL(BASE);
  const remote = `${url.protocol}//x:${wfPushToken}@${url.host}/acme/${WF_REPO}.git`;
  execFileSync('git', ['clone', '--quiet', remote, WF_DIR]);
  wfGit('config', 'user.email', 'ada@acme.dev');
  wfGit('config', 'user.name', 'Ada Owner');
  wfGit('checkout', '--quiet', '-b', WF_BRANCH);
  wfLive = true;
  console.log(`  pushing to acme/${WF_REPO}`);
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
// Self-hosted runners. An organisation's own machine takes the jobs a
// file asks for with `runs-on: [self-hosted, …]`, shaped like GitHub's:
// an org policy, runner groups that admit repositories, a registration
// token exchanged for the runner's own credential, and a runner that
// only ever calls out.
//
// The runner is the REAL `weft-runner` binary, started by this stage
// from the command the settings page shows — not a stub, and not a
// runner the stack pre-registered. Registering a runner and seeing it in
// a table proves the form; a job that a person's own machine took and
// reported is the thing. `scripts/manual-stack.sh` exports RUNNER_BIN;
// without it these stages report the missing prerequisite, the same
// rule as a missing SSH URL.
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
  for (const want of ['Self-hosted runners', 'Runner groups', 'Add a runner'])
    if (!text.includes(want))
      problems.push({ where: stage, kind: 'content', text: `the Runners settings page does not show "${want}"` });
  // One pool — the organisation's own machines — so no control for a
  // second one it could never use.
  if (text.includes('Weft-hosted runners'))
    problems.push({ where: stage, kind: 'content', text: 'the Runners settings page still offers a hosted pool' });
  const existing = await shRunners();
  if (existing.length)
    problems.push({
      where: stage,
      kind: 'harness',
      text: `acme already has ${existing.length} runner(s) (${existing.map((r) => r.name).join(', ')}) — a stack a previous run left behind; bring it up fresh`,
    });
  await shot(page, '16v-runners-settings', 'the runner policy, one default group, no runners');
  shLive = true;
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

await step("runners / a push runs on the organisation's own machine", async () => {
  if (!shLive || !shProc) return;
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
  if (job?.runner?.name !== SH_NAME)
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `the job should record that ${SH_NAME} ran it; it records ${JSON.stringify({ runner: job?.runner })}`,
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
  console.log(`  run ${run.id} passed on ${SH_NAME}`);
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

await step('runners / an owner can turn runners off, and a file that asks for one is told', async () => {
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
    'a file asking for runners the organisation turned off',
  );
  // And back, or every stage below reads as broken.
  const restored = await shPolicy({ self_hosted: 'all' });
  if (restored.status !== 200 || restored.body?.self_hosted !== 'all')
    problems.push({ where: stage, kind: 'behaviour', text: `restoring the policy answered ${restored.status}: ${JSON.stringify(restored.body)}` });
  console.log('  refused by policy; policy restored');
});

await step("runners / a miner on the organisation's own machine is stopped, and the runner keeps listening", async () => {
  if (!shLive || !shProc) return;
  // A renamed program, assembled at runtime so no parser can see it. The
  // machine is the organisation's own, and the runner's watch is
  // protecting it from a stranger's change — so the job stops, with the
  // sentence that says why, and that is all.
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
  // The runner itself is still listening: it killed the job, not itself.
  const still = (await shRunners()).find((r) => r.name === SH_NAME);
  if (shExit !== null || !still || still.state === 'offline')
    problems.push({
      where: stage,
      kind: 'behaviour',
      text: `the runner did not survive stopping a miner (exit ${shExit}, listed as ${JSON.stringify(still?.state)}); it said: ${shOut.slice(-400)}`,
    });
  await page.goto(`${BASE}/acme/${WF_REPO}/checks/runs/${run.id}`, { waitUntil: 'networkidle' });
  await shot(page, '16ag-self-hosted-miner-stopped', 'stopped on their machine; the runner still listening');
  console.log(`  run ${run.id} ${run.state}: ${why}`);
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
  // early `if (!wfLive) return`, so on a stack with no workflow repo the
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
  // it. Here the Insights tab is the observable: it is drawn from the
  // row's `viewer_member` and from nothing else, so once it is on screen
  // the answer has arrived and the Settings absence below means
  // something.
  //
  // A viewer gets the repository — this is the same page a maintainer
  // sees, which is the whole design — and the two gates on it land on
  // opposite sides of them, which is the whole reason there are two.
  //
  // They are a **member**: Insights is theirs to read, because a
  // repository's traffic belongs to the people who own it. They are not
  // an admin: Settings is not.
  await repoTab("Insights")
    .waitFor({ timeout: 10000 })
    .catch(() => undefined);
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

await step("describing a repository", async () => {
  // The description lives under the repository's Settings tab now, an
  // ordinary saved field rather than an edit-in-place on a dashboard
  // screen.
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
