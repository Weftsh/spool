// Coming back from the GitHub App install. The callback lands the
// browser on the dashboard with `?connect=<outcome>`, and for a long
// time only `ok` did anything here: the first real install on weft.sh
// came back `missing` and the screen showed nothing at all — a
// "GitHub is not connected" notice with an Install button leading to
// an app that was already installed. Every outcome that is not `ok`
// is now a sentence with a next step.

import { expect, test } from "@playwright/test";

import { signIn } from "./fixtures";

/// Outcome, the verdict, and the next step — a banner that only
/// says what went wrong is a dead end with better wording.
const SENTENCES: Array<[string, RegExp, RegExp]> = [
  [
    "notyours",
    /did not confirm that the installation is yours/,
    /Start again from Connect GitHub/,
  ],
  ["expired", /already used or has expired/, /Start again from Connect GitHub/],
  [
    "missing",
    /without saying which organization/,
    /Start from Connect GitHub in the organization you meant/,
  ],
  ["which", /more than one organization/, /Start again from the one you meant/],
  [
    "taken",
    /already connected to another organization/,
    /Each installation can serve one organization/,
  ],
  ["error", /GitHub did not answer/, /try again in a moment/],
];

test("every refused connect outcome is said on screen, with what to do", async ({
  page,
}) => {
  await signIn(page);
  for (const [outcome, verdict, next] of SENTENCES) {
    await page.goto(`/dashboard/?connect=${outcome}`);
    const banner = page.getByRole("status").filter({ hasText: verdict });
    await expect(banner).toBeVisible();
    await expect(banner).toContainText(next);
  }
});

test("a good connect, and no connect at all, show no warning", async ({
  page,
}) => {
  await signIn(page);
  await page.goto("/dashboard/?connect=ok&org=acme");
  await expect(page.getByText("Requests today")).toBeVisible();
  await expect(
    page.getByRole("status").filter({ hasText: /GitHub/ }),
  ).toHaveCount(0);
  await page.goto("/dashboard/?connect=nonsense");
  await expect(page.getByText("Requests today")).toBeVisible();
  await expect(
    page.getByRole("status").filter({ hasText: /GitHub/ }),
  ).toHaveCount(0);
});
