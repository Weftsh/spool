import type { ContributionGraph } from "@/api";

/// The contribution graph: a year of squares, one per day.
///
/// This is the page's migration argument made visible. Somebody moving a
/// decade of work off another forge loses the visible half of it on the
/// day they move, and the only honest answer is to render *their
/// commits* — which is what the server does. So the component's job is
/// to make that legible and never to make it look busier than it is.
///
/// Three rules the design system decides for it:
///
/// - **Five steps out of the `--series-1` family, never the amber.**
///   `--accent` is a counterpoint hue and cannot carry a sequential
///   ramp: mixed toward the ground it goes muddy in the middle, and a
///   heat scale whose third step is dimmer than its second is a scale
///   that reads backwards. The steps are mixed in OKLab from the one
///   token, so a palette change moves all five at once and there is no
///   hex anywhere in this file.
/// - **Never colour alone.** Every square carries a `title` naming the
///   date and the count, and the legend under the grid is labelled
///   "Less"/"More" — the same rule `SyncBadge` and `LanguageBar` are
///   written to.
/// - **`min-w-0` on the flex parent.** Without it a 53-column grid
///   widens the page instead of scrolling inside its own box, and the
///   walkthrough audits `documentElement.scrollWidth`.

/// How many colour steps the ramp has, counting the empty square.
///
/// Five is what the token family can carry legibly: mixed from one hue
/// toward the well, six steps put two of them within a shade of each
/// other, and two steps a reader cannot tell apart are worse than one
/// step fewer.
export const STEPS = 5;

/// Which of the five steps a day's count lands on, given the busiest
/// day in the same graph.
///
/// **Relative to the person's own year, which is what GitHub does and
/// is not what I first built.** The fixed ramp that was here argued that
/// two graphs should be comparable — and it is a real argument, but it
/// is the wrong trade for us, and reading the real thing is what settled
/// it.
///
/// Measured, not remembered: `github.com/mojombo` in 2024 has 34
/// contributions with a busiest day of 7, and his squares run 1→L1,
/// 2–3→L2, 4→L3, 7→L4. `github.com/dtolnay` in 2024 has 4,984 with a
/// busiest day of 100, and his boundary sits between 25 and 26.
/// `ceil(4 × count / max)` reproduces every one of 324 day/level pairs
/// across both, which is where this formula comes from.
///
/// Why it matters more for us than for GitHub: our counts are already
/// lower than theirs for the same year, because we count verified commit
/// authorship and they also count issues, pull requests and reviews. A
/// fixed ramp on top of that would understate a migrating maintainer
/// twice over — a year of real work rendering as the palest step,
/// reading as "barely used" on the page whose whole job is to say the
/// opposite. A quiet year is still allowed to look quiet: it has few
/// squares, which is the honest signal, rather than many pale ones.
export function heatStep(count: number, max: number): number {
  if (count <= 0) return 0;
  // A day with any work on it is never step 0, whatever the ceiling —
  // an invisible contribution is worse than an imprecise one.
  if (max <= 0) return 1;
  return Math.min(
    STEPS - 1,
    Math.max(1, Math.ceil((STEPS - 1) * (count / max))),
  );
}

/// The colour for a step, as a token expression.
///
/// `color-mix` toward `--surface-2` rather than five separate tokens,
/// because the ramp *is* one hue at five saturations and spelling it out
/// five times in `shared/tokens.css` would be five places for it to
/// drift. Step 0 is the bare well, so an empty day is the same surface
/// as the panel behind it and reads as absence rather than as a very
/// faint value.
export function heatColor(step: number): string {
  if (step <= 0) return "var(--surface-2)";
  const pct = [0, 25, 45, 70, 100][Math.min(step, STEPS - 1)];
  return `color-mix(in oklab, var(--series-1) ${pct}%, var(--surface-2))`;
}

/// A day number (days since the epoch) as `YYYY-MM-DD`, matching what
/// the server sends. Used to fill the gaps the server deliberately does
/// not send: a day with nothing on it is absent from the response, and
/// rendering 365 zeroes over the wire would be sending nothing 365
/// times.
export function isoOfDay(day: number): string {
  return new Date(day * 86_400_000).toISOString().slice(0, 10);
}

/// One square.
export interface Cell {
  date: string;
  day: number;
  count: number;
  /// The public repositories behind the count, when there are any. The
  /// difference between `count` and the sum of these is private work,
  /// and it is the only thing ever said about it.
  repos: { org: string; name: string; count: number }[];
}

/// Every day in the window, in order, with the server's counts filled in.
///
/// The server sends only the days that have something on them, so the
/// gaps are made here. Built by *day number* rather than by walking
/// dates: date arithmetic across a daylight-saving boundary is how a
/// grid grows or loses a column once a year, in one hemisphere, for two
/// weeks.
export function cells(graph: ContributionGraph): Cell[] {
  const by = new Map(graph.days.map((d) => [d.date, d]));
  const from = Math.round(Date.parse(`${graph.from}T00:00:00Z`) / 86_400_000);
  const to = Math.round(Date.parse(`${graph.to}T00:00:00Z`) / 86_400_000);
  const out: Cell[] = [];
  for (let day = from; day <= to; day++) {
    const date = isoOfDay(day);
    const hit = by.get(date);
    out.push({
      date,
      day,
      count: hit?.count ?? 0,
      repos: hit?.repos ?? [],
    });
  }
  return out;
}

/// The columns of the grid: weeks, Sunday-first, the way every reader
/// already expects one.
///
/// The first column is short whenever the window does not begin on a
/// Sunday, and that is deliberate — padding it with fake empty days
/// would draw squares for dates outside the window, which a reader would
/// hover and be told about.
export function weeks(list: Cell[]): Cell[][] {
  const out: Cell[][] = [];
  for (const cell of list) {
    // 1970-01-01 was a Thursday, so `day + 4` puts Sunday at 0.
    const weekday = (((cell.day + 4) % 7) + 7) % 7;
    if (out.length === 0 || weekday === 0) out.push([]);
    out[out.length - 1].push(cell);
  }
  return out;
}

/// The month labels above the grid: a name and how many week columns it
/// owns, in column order.
///
/// A month is labelled at its **first full week** — the first column
/// whose Sunday falls inside it — which is how GitHub aligns them, and
/// the reason a label never sits above a column that is mostly the
/// previous month. A month that owns fewer than two columns in the
/// window gets no label at all: two names three pixels apart are less
/// legible than one, and the first partial column at the very start of
/// the window is exactly that case.
export function monthSpans(
  columns: Cell[][],
): { label: string; span: number }[] {
  const NAMES = [
    "Jan",
    "Feb",
    "Mar",
    "Apr",
    "May",
    "Jun",
    "Jul",
    "Aug",
    "Sep",
    "Oct",
    "Nov",
    "Dec",
  ];
  const out: { label: string; span: number }[] = [];
  for (const week of columns) {
    // The column belongs to the month its *first* day is in.
    const month = Number(week[0].date.slice(5, 7)) - 1;
    const last = out.length > 0 ? out[out.length - 1] : null;
    if (last && last.label === NAMES[month]) {
      last.span += 1;
    } else {
      out.push({ label: NAMES[month], span: 1 });
    }
  }
  // A one-column month is a sliver at an edge of the window; label it
  // and the name overhangs its neighbour.
  return out.map((m, i) =>
    m.span < 2 && (i === 0 || i === out.length - 1)
      ? { label: "", span: m.span }
      : m,
  );
}

/// What one square says when a reader hovers it — and what a screen
/// reader is given, since the colour says nothing on its own.
export function cellLabel(cell: Cell): string {
  const work =
    cell.count === 1 ? "1 contribution" : `${cell.count} contributions`;
  if (cell.count === 0) return `No contributions on ${cell.date}`;
  const named = cell.repos.map((r) => `${r.org}/${r.name}`).join(", ");
  // A day whose count exceeds what the named repositories account for
  // has private work in it. It is named as *private work*, with no
  // repository, no title and no link — that is the whole contract, and
  // saying "and more" instead would invite a reader to guess.
  const shown = cell.repos.reduce((sum, r) => sum + r.count, 0);
  const hidden = cell.count - shown;
  const parts = [named, hidden > 0 ? "private work" : ""].filter(Boolean);
  return parts.length > 0
    ? `${work} on ${cell.date} — ${parts.join(", ")}`
    : `${work} on ${cell.date}`;
}

export function ContributionGraphView(props: { graph: ContributionGraph }) {
  const { graph } = props;
  const all = cells(graph);
  const busiest = all.reduce((n, c) => Math.max(n, c.count), 0);
  const columns = weeks(all);
  return (
    <section className="min-w-0">
      <div className="flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1">
        {/* **"commits", not "contributions"**, and the difference is the
            most consequential word on this page.

            GitHub's identical-looking header counts commits, pull
            requests, issues and reviews together — for `dtolnay` in
            2024 that mix is 81% commits, 12% pull requests, 5% code
            review, 2% issues, and for a maintainer who mostly reviews
            it is far less commit-heavy than that. We count verified
            commit authorship and nothing else, so the same year will
            always read lower here.

            A migrating maintainer comparing two numbers under the same
            word will conclude we lost their history, which is exactly
            the fear this feature exists to answer. Naming what is
            counted turns a smaller number from evidence of a broken
            import into a plain statement of scope — and leaves the word
            free to widen honestly the day we count more. */}
        <h2 className="text-sm font-semibold text-ink">
          {/* Numbers always render in mono (DESIGN.md). */}
          <span className="font-mono">{graph.total}</span>{" "}
          {graph.total === 1 ? "commit" : "commits"}
        </h2>
        <p className="text-xs text-ink-3">
          {graph.from} to {graph.to}
        </p>
      </div>
      {/* The scroll lives on this box, not on the page: a 53-column grid
          is wider than a phone, and a container that widens instead is
          exactly what the walkthrough's overflow audit fails a build
          for. */}
      <div className="mt-3 min-w-0 overflow-x-auto">
        <div
          className="flex gap-[3px]"
          role="img"
          aria-label={`${graph.total} commits from ${graph.from} to ${graph.to}`}
        >
          {/* Mon / Wed / Fri only, as GitHub shows them: seven labels in
              a 10px row pitch collide, and three are enough to orient a
              reader who already knows the week runs downwards. */}
          <div
            aria-hidden
            className="mr-1 flex shrink-0 flex-col gap-[3px] pt-[17px] text-[9px] leading-[10px] text-ink-3"
          >
            {["", "Mon", "", "Wed", "", "Fri", ""].map((d, i) => (
              <span key={i} className="h-[10px]">
                {d}
              </span>
            ))}
          </div>
          <div className="flex flex-col gap-[4px]">
            <div className="flex gap-[3px] text-[9px] leading-[10px] text-ink-3">
              {monthSpans(columns).map((m, i) => (
                <span
                  key={i}
                  aria-hidden
                  className="shrink-0 overflow-hidden whitespace-nowrap"
                  // Data, not decoration: a month's label is exactly as
                  // wide as the weeks it owns, so it sits over its own
                  // columns instead of drifting.
                  style={{ width: `${m.span * 13 - 3}px` }}
                >
                  {m.label}
                </span>
              ))}
            </div>
            <div className="flex gap-[3px]">
              {columns.map((week) => (
                <div key={week[0].date} className="flex flex-col gap-[3px]">
                  {week.map((cell) => (
                    <span
                      key={cell.date}
                      data-testid="contribution-cell"
                      data-date={cell.date}
                      data-count={cell.count}
                      title={cellLabel(cell)}
                      className="size-[10px] rounded-[2px]"
                      // Data and a token expression. No hex reaches a
                      // component (DESIGN.md).
                      style={{
                        background: heatColor(heatStep(cell.count, busiest)),
                      }}
                    />
                  ))}
                </div>
              ))}
            </div>
          </div>
        </div>
      </div>
      <div className="mt-2 flex items-center gap-2 text-xs text-ink-3">
        {/* The one fact about private work this page ever states, and it
            is stated rather than implied: a reader has to be able to
            tell a quiet fortnight from an opted-out one. */}
        <span className="mr-auto">
          {graph.private_included
            ? "Commits you authored, private repositories included as daily totals."
            : "Commits you authored in public repositories."}
        </span>
        <span>Less</span>
        {Array.from({ length: STEPS }, (_, step) => (
          <span
            key={step}
            aria-hidden
            className="size-[11px] rounded-[2px]"
            style={{ background: heatColor(step) }}
          />
        ))}
        <span>More</span>
      </div>
    </section>
  );
}
