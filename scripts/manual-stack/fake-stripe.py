#!/usr/bin/env python3
"""A Stripe-shaped responder for the manual browser pass.

Answers the endpoints the product calls, so the billing screens can be
driven in a real browser without a Stripe account — and serves the two
hosted pages a person is sent to, so the pass can *finish* the trip:

  * `/pay/<session>` stands in for Checkout — subscription mode only,
    like an account on Managed Payments (Stripe as merchant of record),
    which has no setup mode at all and refuses one with the sentence
    below. The page shows the seats and the price, takes a promotion
    code (`WEFT100` is the one it knows: 100% off), and "Subscribe"
    delivers `customer.subscription.created`, then the completion, then
    `invoice.paid` — Stripe's order; "Cancel" goes to `cancel_url`.
  * `/portal/<customer>` stands in for the billing portal. Its buttons
    deliver `invoice.payment_failed`, `invoice.paid` and
    `customer.subscription.deleted`, so the dunning states can be looked
    at with a person's eyes rather than only asserted by `cargo test`.
  * `/meters` lists every usage report the server sent to
    `POST /v1/billing/meter_events` — meter, customer, value, identifier,
    timestamp — so a person driving the stack can see the overage the
    dashboard shows reach the provider.

`stratum-testkit`'s `fake_stripe` is the authority on the API shapes and
is what `cargo test` uses; the bodies here are the same `2025-03-31.basil`
ones, with ids derived the same way (`cus_<org>`, `sub_<org>`,
`si_<org>`, and `si_min_<org>` / `si_egr_<org>` / `si_sto_<org>` for the
four metered items every subscription carries after its seat item).
Keep the two in step: if the product starts calling an endpoint, teach
the Rust one first and mirror it here. The metered price ids the stack
configures the server with are `price_minutes`, `price_egress`,
`price_storage` and `price_packages`, and a Checkout that puts a
quantity on one of them is refused the way Stripe refuses it.

Environment:
  FAKE_STRIPE_PORT            listen port (59100)
  FAKE_STRIPE_WEBHOOK_URL     where signed events are delivered
                              (http://127.0.0.1:8080/webhooks/stripe)
  FAKE_STRIPE_WEBHOOK_SECRET  what they are signed with (whsec_manual)

A declined card is Stripe's page's business now and has no stand-in
here: nothing the product calls can meet one.
"""
import hashlib
import hmac
import html
import json
import os
import time
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(os.environ.get("FAKE_STRIPE_PORT", "59100"))
WEBHOOK_URL = os.environ.get("FAKE_STRIPE_WEBHOOK_URL", "http://127.0.0.1:8080/webhooks/stripe")
WEBHOOK_SECRET = os.environ.get("FAKE_STRIPE_WEBHOOK_SECRET", "whsec_manual")
PERIOD_END_SECS = 1_900_000_000

STATE = {"sessions": {}, "portals": {}, "events": 0, "meter_events": [], "added_items": []}
PROMO_CODE = "WEFT100"
# (price id, item id prefix): the metered items after the seat item, in
# Checkout order — the same ids `stratum_testkit::fake_stripe` mints.
METERED = [
    ("price_minutes", "si_min"),
    ("price_egress", "si_egr"),
    ("price_storage", "si_sto"),
    ("price_packages", "si_pkg"),
]


def rest(ref, prefix):
    return ref[len(prefix):] if ref.startswith(prefix) else ref


def subscription_object(org, status, seats, discounts=()):
    period = {"current_period_start": PERIOD_END_SECS - 30 * 86400, "current_period_end": PERIOD_END_SECS}
    items = [{"id": f"si_{org}", "object": "subscription_item", "quantity": seats,
              "price": {"id": "price_seat", "object": "price"}, **period}]
    for price, prefix in METERED:
        items.append({"id": f"{prefix}_{org}", "object": "subscription_item",
                      "price": {"id": price, "object": "price", "recurring": {"usage_type": "metered"}}, **period})
    return {
        "id": f"sub_{org}", "object": "subscription", "customer": f"cus_{org}",
        "status": status, "metadata": {"org_id": org},
        "discounts": list(discounts),
        "items": {"object": "list", "data": items},
    }


def event(kind, obj):
    STATE["events"] += 1
    return {"id": f"evt_fake_{int(time.time())}_{STATE['events']}", "type": kind,
            "data": {"object": obj}}


def deliver(ev):
    """Sign and POST one event to the server, Stripe's way."""
    body = json.dumps(ev).encode()
    t = int(time.time())
    v1 = hmac.new(WEBHOOK_SECRET.encode(), f"{t}.".encode() + body, hashlib.sha256).hexdigest()
    req = urllib.request.Request(WEBHOOK_URL, data=body, method="POST", headers={
        "content-type": "application/json", "stripe-signature": f"t={t},v1={v1}"})
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            return r.status, r.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()
    except Exception as e:  # noqa: BLE001 — shown on the page
        return 0, str(e)


def page(title, body):
    return f"""<!doctype html><meta charset=utf-8><title>{html.escape(title)}</title>
<style>body{{font:16px system-ui;max-width:32em;margin:4em auto;padding:0 1em}}
button,a.btn{{display:inline-block;padding:.6em 1.2em;border:1px solid #888;border-radius:6px;
background:#fff;color:#000;text-decoration:none;font:inherit;cursor:pointer;margin:.3em .3em 0 0}}
.primary{{background:#635bff;border-color:#635bff;color:#fff}}</style>
<p style="color:#666">fake stripe — nothing here is real</p>
<h1>{html.escape(title)}</h1>{body}"""


def subscribe_page(path, s, query):
    """The subscription-mode Checkout: seats, price, the saved card, and
    the promotion-code box the real page has when
    `allow_promotion_codes` is on."""
    seats = s["seats"]
    total = 4 * seats
    bad = query.get("bad", [""])[0]
    promo = f"""<p><label>Promotion code <input name=promotion_code value="{html.escape(bad)}"
placeholder="e.g. {PROMO_CODE}"></label></p>""" if s["allow_promotion_codes"] else ""
    err = "<p style=\"color:#b00\">That code is not valid.</p>" if bad else ""
    body = f"""<p>Subscribe organisation <code>{html.escape(s['org'])}</code>:
<b>{seats} {'seat' if seats == 1 else 'seats'}</b> at $4 a month each — <b>${total}/month</b>,
charged to the card on file (<code>•••• 4242</code>, prefilled).</p>
{err}<form method=post action="{html.escape(path)}/complete">{promo}
<button class=primary>Subscribe</button>
<a class=btn href="{html.escape(s['cancel_url'])}">Cancel</a></form>"""
    return page("Subscribe", body)


class H(BaseHTTPRequestHandler):
    def _send(self, status, body, ctype="application/json", extra=()):
        raw = body if isinstance(body, bytes) else body.encode()
        self.send_response(status)
        self.send_header("content-type", ctype)
        self.send_header("content-length", str(len(raw)))
        for k, v in extra:
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(raw)

    def _json(self, status, obj):
        self._send(status, json.dumps(obj))

    def _redirect(self, to):
        self._send(303, b"", "text/plain", [("location", to)])

    def _form(self):
        n = int(self.headers.get("content-length", 0))
        return {k: v[0] for k, v in urllib.parse.parse_qs(self.rfile.read(n).decode()).items()}

    def do_GET(self):
        u = urllib.parse.urlsplit(self.path)
        p = u.path
        if p == "/v1/ping":
            return self._json(200, {"ok": True})
        if p.startswith("/pay/"):
            s = STATE["sessions"].get(rest(p, "/pay/"))
            if not s:
                return self._send(404, "no such session", "text/plain")
            return self._send(200, subscribe_page(p, s, urllib.parse.parse_qs(u.query)), "text/html")
        if p.startswith("/portal/"):
            cus = rest(p, "/portal/")
            back = STATE["portals"].get(cus, "/")
            body = f"""<p>Customer <code>{html.escape(cus)}</code>. Each button delivers the
event a real portal action would cause, then returns to Stratum.</p>
<form method=post action="{html.escape(p)}">
<button name=do value=fail>Fail the next payment</button>
<button name=do value=pay>Pay the open invoice</button>
<button name=do value=cancel>Cancel the subscription</button>
<a class=btn href="{html.escape(back)}">Back to Stratum</a></form>"""
            return self._send(200, page("Billing portal", body), "text/html")
        if p == "/meters":
            rows = "".join(
                f"<tr><td>{html.escape(e['event_name'])}</td><td><code>{html.escape(e['customer'])}</code></td>"
                f"<td>{html.escape(e['value'])}</td><td><code>{html.escape(e['identifier'])}</code></td>"
                f"<td>{html.escape(e['timestamp'])}</td><td>{html.escape(e['received'])}</td></tr>"
                for e in STATE["meter_events"])
            body = (f"<p>{len(STATE['meter_events'])} usage report(s) the server sent to "
                    "<code>POST /v1/billing/meter_events</code>, oldest first. A real Stripe counts one "
                    "identifier once per 24 hours; this page shows every arrival so a retry is visible.</p>"
                    "<table><tr><th>meter</th><th>customer</th><th>value</th><th>identifier</th>"
                    f"<th>timestamp</th><th>received</th></tr>{rows}</table>")
            if STATE["added_items"]:
                body += ("<p>Metered items added outside Checkout:</p><ul>"
                         + "".join(f"<li><code>{html.escape(i)}</code></li>" for i in STATE["added_items"]) + "</ul>")
            return self._send(200, page("Meter events", body), "text/html")
        self._json(404, {"error": {"type": "invalid_request_error", "code": "resource_missing",
                                   "message": f"Unrecognized request URL (GET: {p})"}})

    def do_POST(self):
        u = urllib.parse.urlsplit(self.path)
        p = u.path
        form = self._form()
        org = form.get("metadata[org_id]", "fake")
        if p == "/v1/customers":
            return self._json(200, {"id": f"cus_{org}", "object": "customer"})
        if p == "/v1/checkout/sessions":
            if form.get("mode") != "subscription":
                return self._json(400, {"error": {"type": "invalid_request_error", "message":
                    f"Invalid mode: {form.get('mode')}. Managed Payments, which is enabled by default on your "
                    "account, only supports mode: subscription or mode: payment."}})
            prices = []
            while f"line_items[{len(prices)}][price]" in form:
                prices.append(form[f"line_items[{len(prices)}][price]"])
            for i in range(1, len(prices)):
                if f"line_items[{i}][quantity]" in form:
                    return self._json(400, {"error": {"type": "invalid_request_error",
                                                      "param": f"line_items[{i}][quantity]",
                                                      "message": "You cannot specify `quantity` for a metered price."}})
            cs = f"cs_fake_{len(STATE['sessions']) + 1}"
            STATE["sessions"][cs] = {"org": form.get("client_reference_id", ""),
                                     "prices": prices,
                                     "customer": form.get("customer", ""),
                                     "mode": form.get("mode", "setup"),
                                     "seats": int(form.get("line_items[0][quantity]", "1")),
                                     "allow_promotion_codes": form.get("allow_promotion_codes") == "true",
                                     "success_url": form.get("success_url", "/"),
                                     "cancel_url": form.get("cancel_url", "/")}
            return self._json(200, {"id": cs, "object": "checkout.session", "mode": form.get("mode"),
                                    "allow_promotion_codes": form.get("allow_promotion_codes") == "true",
                                    "url": f"http://127.0.0.1:{PORT}/pay/{cs}"})
        if p == "/v1/billing_portal/sessions":
            cus = form.get("customer", "cus_fake")
            STATE["portals"][cus] = form.get("return_url", "/")
            return self._json(200, {"id": "bps_fake", "url": f"http://127.0.0.1:{PORT}/portal/{cus}"})
        if p.startswith("/v1/subscription_items/"):
            q = form.get("quantity")
            return self._json(200, {"id": rest(p, "/v1/subscription_items/"),
                                    "object": "subscription_item",
                                    "quantity": int(q) if q else None})
        if p == "/v1/subscription_items":
            sub, price = form.get("subscription", ""), form.get("price", "")
            if not sub or not price:
                return self._json(400, {"error": {"type": "invalid_request_error",
                                                  "message": "Missing required param: subscription and price."}})
            item = f"si_{price}_{sub}"
            STATE["added_items"].append(f"{item} ({price} on {sub})")
            return self._json(200, {"id": item, "object": "subscription_item", "subscription": sub,
                                    "price": {"id": price, "object": "price"}})
        if p == "/v1/billing/meter_events":
            missing = [k for k in ("event_name", "payload[stripe_customer_id]", "payload[value]") if not form.get(k)]
            if missing:
                return self._json(400, {"error": {"type": "invalid_request_error",
                                                  "message": f"Missing required param: {', '.join(missing)}."}})
            ts = form.get("timestamp") or str(int(time.time()))
            ident = form.get("identifier", "")
            repeat = bool(ident) and any(e["identifier"] == ident for e in STATE["meter_events"])
            STATE["meter_events"].append({
                "event_name": form["event_name"], "customer": form["payload[stripe_customer_id]"],
                "value": form["payload[value]"], "identifier": form.get("identifier", ""), "timestamp": ts,
                "received": time.strftime("%H:%M:%S") + (" (repeat, refused 400)" if repeat else "")})
            if repeat:
                # What the real account answers a repeated identifier
                # with (observed 2026-09-07); the server reads it as success.
                return self._json(400, {"error": {"type": "invalid_request_error",
                                                  "message": f"An event already exists with identifier {ident}."}})
            return self._json(200, {"object": "billing.meter_event", "identifier": form.get("identifier"),
                                    "event_name": form["event_name"], "livemode": False, "timestamp": int(ts),
                                    "payload": {"stripe_customer_id": form["payload[stripe_customer_id]"],
                                                "value": form["payload[value]"]}})
        # The hosted pages' buttons.
        if p.startswith("/pay/") and p.endswith("/complete"):
            cs = rest(p, "/pay/")[: -len("/complete")]
            s = STATE["sessions"].get(cs)
            if not s:
                return self._send(404, "no such session", "text/plain")
            return self._complete_subscription(cs, s, form)
        if p.startswith("/portal/"):
            cus = rest(p, "/portal/")
            org = rest(cus, "cus_")
            do = form.get("do")
            inv = {"id": "in_fake", "object": "invoice", "customer": cus,
                   "parent": {"type": "subscription_details",
                              "subscription_details": {"subscription": f"sub_{org}"}}}
            ev = {"fail": event("invoice.payment_failed", inv),
                  "pay": event("invoice.paid", inv),
                  "cancel": event("customer.subscription.deleted", subscription_object(org, "canceled", 1))}.get(do)
            if not ev:
                return self._send(400, "unknown action", "text/plain")
            st, out = deliver(ev)
            if st != 200:
                return self._send(502, page("Stratum refused the event",
                    f"<p><b>{st}</b></p><pre>{html.escape(out)}</pre>"), "text/html")
            return self._redirect(STATE["portals"].get(cus, "/"))
        self._json(404, {"error": {"type": "invalid_request_error", "code": "resource_missing",
                                   "message": f"Unrecognized request URL (POST: {p})"}})

    def _complete_subscription(self, cs, s, form):
        """Stripe's three events for a finished subscription Checkout, in
        its order, each answered before the next is sent."""
        code = (form.get("promotion_code") or "").strip()
        if code and code != PROMO_CODE:
            return self._redirect(f"/pay/{cs}?bad={urllib.parse.quote(code)}")
        org = s["org"]
        discounts = [f"di_fake_{code}"] if code else []
        sub = subscription_object(org, "active", s["seats"], discounts)
        inv = {"id": "in_fake", "object": "invoice", "customer": s["customer"], "status": "paid",
               "total": 0 if code else 400 * s["seats"],
               "parent": {"type": "subscription_details",
                          "subscription_details": {"subscription": sub["id"], "metadata": {}}}}
        for ev in (event("customer.subscription.created", sub),
                   event("checkout.session.completed", {
                       "id": cs, "object": "checkout.session", "mode": "subscription",
                       "status": "complete", "payment_status": "paid",
                       "allow_promotion_codes": s["allow_promotion_codes"],
                       "client_reference_id": org, "customer": s["customer"],
                       "setup_intent": None, "subscription": sub["id"]}),
                   event("invoice.paid", inv)):
            st, out = deliver(ev)
            if st != 200:
                return self._send(502, page("The subscription did not land",
                    f"<p>Stratum answered <b>{st}</b> to <code>{ev['type']}</code>:</p>"
                    f"<pre>{html.escape(out)}</pre>"), "text/html")
        return self._redirect(s["success_url"])

    def log_message(self, *a):
        pass


if __name__ == "__main__":
    # Threaded: the fake calls the server (three webhooks per finished
    # page) while the browser's request to this port is still open, and
    # a server that handles one request at a time deadlocks on that
    # shape the moment a webhook needs an answer from here.
    ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
