// Finish a Stripe **test-mode** Checkout in a browser, so
// `scripts/manual-stripe.sh fixtures` can record a real
// `checkout.session.completed`.
//
// Why this exists: the account is on Managed Payments, which has no
// setup-mode Checkout, so the only way a card reaches a customer is a
// person finishing the subscription Checkout page. That made
// `checkout.session.completed` the one event the contract could never
// observe unattended — and it is the body the product reads to learn
// which subscription a Checkout opened. A fixture that has never been
// recorded is a fixture nothing is pinned to.
//
// This is not a claim that Checkout works for a human; it is a way to
// obtain the body. The card is Stripe's published test number and this
// refuses to run against anything but a test-mode session.
//
//   node tools/stripe-checkout.mjs "<checkout url>"
import { chromium } from "playwright";
import { readFileSync } from "node:fs";

const url = process.argv[2] || readFileSync("/tmp/checkout-url.txt", "utf8").trim();
if (!/^https:\/\/checkout\.stripe\.com\//.test(url)) {
  console.error("not a Stripe Checkout URL");
  process.exit(2);
}

const CARD = "4242424242424242";
const exec = process.env.CHROMIUM_PATH || undefined;
const browser = await chromium.launch({ headless: true, executablePath: exec });
const page = await browser.newPage();
const problems = [];

try {
  await page.goto(url, { waitUntil: "domcontentloaded", timeout: 60000 });

  // Checkout renders a skeleton first and fills it in later, so wait for
  // something real before reading the page at all. Reading too early got
  // a loader with no text in it, and the guard below then refused a
  // perfectly good sandbox session — it was right to refuse what it saw;
  // it was being shown the wrong thing.
  await page.getByRole("button", { name: /subscribe/i }).first()
    .waitFor({ state: "visible", timeout: 60000 });

  // A non-live Checkout says so on the page. Stripe labels these
  // "Sandbox" now and said "TEST MODE" before; accept either and refuse
  // anything that says neither, rather than type a card number into a
  // live Checkout.
  const body = (await page.innerText("body")).toLowerCase();
  if (!body.includes("test mode") && !body.includes("sandbox")) {
    throw new Error("this Checkout says neither TEST MODE nor Sandbox — refusing to fill it");
  }

  // Link (and Apple Pay) is the default payment UI and the card fields
  // do not exist until Card is chosen.
  // Choose Card. Link (and Apple Pay) is the default and the card
  // fields do not exist until Card is picked. The control is an input
  // named `payment-method-accordion-item-title` — neither the visible
  // word "Card", which sits under a cover element that swallows the
  // click, nor a radio, which is what it looks like.
  const items = page.locator('input[name="payment-method-accordion-item-title"]');
  if (await items.count().catch(() => 0)) {
    await items.first().click({ force: true });
  }

  const fill = async (name, value) => {
    const el = page.locator(`input[name="${name}"]`).first();
    await el.waitFor({ state: "visible", timeout: 30000 });
    await el.fill(value);
  };

  // Email is present when the session did not pin a customer email.
  const email = page.locator('input[name="email"]').first();
  if (await email.count().catch(() => 0)) {
    if (await email.isVisible().catch(() => false)) {
      await email.fill("manual-gate@example.com");
    }
  }

  await fill("cardNumber", CARD);
  await fill("cardExpiry", "12 / 34");
  await fill("cardCvc", "123");
  const name = page.locator('input[name="billingName"]').first();
  if (await name.isVisible().catch(() => false)) await name.fill("Manual Gate");
  // The whole billing address, not just the postal code. Address line 1
  // and City are required, and leaving them empty simply does nothing
  // when Subscribe is pressed — no error thrown, no navigation, just a
  // form that stays put until the wait times out.
  for (const [field, value] of [
    ["billingAddressLine1", "500 Test Street"],
    ["billingLocality", "San Francisco"],
    ["billingPostalCode", "94107"],
  ]) {
    const el = page.locator(`input[name="${field}"]`).first();
    if (await el.isVisible().catch(() => false)) await el.fill(value);
  }

  // Turn off "Save my information for faster checkout". It is ticked by
  // default and makes the phone number required; leaving it on stops the
  // form with a red phone field and no error anywhere else, which reads
  // as the card being rejected. It also means not creating a Link
  // account for a throwaway gate run.
  const savePass = page.locator('input[name="enableStripePass"]').first();
  if (await savePass.isChecked().catch(() => false)) {
    await savePass.click({ force: true });
  }

  await page.locator('button[type="submit"]').first().click();

  // Checkout leaves its own domain when it succeeds, and the place it
  // goes is the contract script's own short-lived listener. That
  // listener may already be gone, so the navigation can end in
  // ERR_CONNECTION_REFUSED — which is the *success* path seen from the
  // browser, not a failure. What matters is that the URL is no longer
  // Stripe's, so poll for that rather than waiting on a load event.
  const deadline = Date.now() + 120000;
  let left = false;
  while (Date.now() < deadline) {
    let host = "";
    try {
      host = new URL(page.url()).hostname;
    } catch {}
    if (host && !host.endsWith("checkout.stripe.com")) {
      left = true;
      break;
    }
    await page.waitForTimeout(1000);
  }
  if (!left) throw new Error("Checkout never left stripe.com — the form did not submit");
  console.log("checkout completed ->", page.url().slice(0, 120));
} catch (e) {
  problems.push(String(e && e.message ? e.message : e));
  try {
    await page.screenshot({ path: "/tmp/checkout-failed.png", fullPage: true });
    console.error("screenshot: /tmp/checkout-failed.png");
  } catch {}
} finally {
  await browser.close();
}

if (problems.length) {
  console.error("FAILED:", problems.join("; "));
  process.exit(1);
}
