import { Building2, Link as LinkIcon, MapPin } from "lucide-react";
import { OwnerAvatar, type OwnerKind } from "@/components/owner-avatar";
import { PROSE_LINK } from "@/lib/links";

/// A link as the profile publishes it, which is a label nobody may have
/// given and a URL somebody typed.
export interface RailLink {
  label: string | null;
  url: string;
}

/// The URL, if it is one a browser may follow, and `null` otherwise.
///
/// The control plane already refuses anything but `http`/`https` at the
/// write (`profiles::clean_link`), so on today's rows this can only
/// return the URL it was given. It is here anyway because this is the
/// last place the string is a string before it becomes an `href`: the
/// column is 300 characters of somebody's typing, the validation that
/// protects it lives in another process and another language, and the
/// failure mode if the two ever disagree is script execution on a page
/// any stranger can open. A guard on a security boundary is worth its
/// three lines even when its only caller is already correct.
///
/// It is an allowlist, not a denylist, and that is the whole of the
/// security argument: a list of schemes to refuse has to be complete,
/// and `javascript:`, `data:`, `vbscript:` and whatever a browser ships
/// next year are not a list anybody finishes. Two schemes are allowed
/// and everything else — known, unknown, or newly invented — is refused
/// by default.
///
/// The `trim()` is not part of that argument, and it would be easy to
/// tell yourself it was. Leading whitespace cannot smuggle a scheme
/// past an allowlist: `" javascript:…"` fails the `startsWith` check
/// with or without it, and a test asserting otherwise proves nothing.
/// What the trim actually prevents is the opposite mistake — a stored
/// `" https://example.com"` refused as if it were hostile, and the
/// owner's real link silently dropped from their profile. It is a
/// false-negative fix, and `profile-rail.test.ts` pins it as one.
///
/// React refuses a `javascript:` href on its own besides, replacing it
/// with a throwing stub. That is a third lock and not a reason to skip
/// this one: it is a framework's behaviour rather than a promise to us,
/// and the link is still rendered and still clickable when it applies.
export function httpUrl(url: string): string | null {
  const trimmed = url.trim();
  const lower = trimmed.toLowerCase();
  if (!lower.startsWith("http://") && !lower.startsWith("https://")) {
    return null;
  }
  return trimmed;
}

/// What a link says when its owner did not label it.
///
/// The bare URL, minus the scheme and a trailing slash — GitHub's own
/// treatment, and the reason is legibility rather than taste: a 300
/// character URL in a 296px rail is an ellipsis with no information in
/// it, and the host is the part a reader is deciding about.
export function linkLabel(link: RailLink): string {
  const label = link.label?.trim();
  if (label) return label;
  return link.url
    .trim()
    .replace(/^https?:\/\//i, "")
    .replace(/\/$/, "");
}

/// One line of the metadata list: a glyph and a value.
///
/// The glyph is `aria-hidden` and the value carries the text, so a
/// screen reader reads "Berlin" rather than "map pin Berlin"
/// (FORGE-UX §5).
function MetaItem(props: { icon: React.ReactNode; children: React.ReactNode }) {
  return (
    <li className="flex min-w-0 items-center gap-2 text-sm text-ink-2">
      <span className="shrink-0 text-ink-3">{props.icon}</span>
      <span className="min-w-0 truncate">{props.children}</span>
    </li>
  );
}

/// The identity rail: who this namespace is.
///
/// Every field is optional and every one is omitted rather than zeroed
/// or placeholdered when it is absent. An empty "Company" row with a
/// dash in it tells a reader nothing and costs them a line to work that
/// out; the absence of the row is the same information, rendered
/// honestly. That is FORGE-UX's rule about sections that lead nowhere,
/// applied one field at a time.
///
/// What the spec puts here and this does not render — Follow / Edit
/// profile, `N followers · N following` — is not an oversight: there is
/// no following substrate and no self-service edit route yet, and a
/// button that cannot do anything is the thing the rule forbids.
export function ProfileRail(props: {
  handle: string;
  displayName?: string | null;
  pronouns?: string | null;
  bio?: string | null;
  company?: string | null;
  location?: string | null;
  links?: RailLink[];
  /// `human` or `agent`, as the control plane spells it. An agent gets
  /// square avatar corners and never borrows a person's look.
  kind?: string | null;
}) {
  const { handle } = props;
  const name = props.displayName?.trim() || handle;
  const kind: OwnerKind = props.kind === "agent" ? "agent" : "user";
  // The handle is only worth its own line when the name above it is
  // saying something different.
  const secondary = [name === handle ? null : handle, props.pronouns?.trim()]
    .filter(Boolean)
    .join(" · ");
  const links = (props.links ?? [])
    .map((l) => ({ ...l, href: httpUrl(l.url) }))
    .filter((l) => l.href !== null);

  return (
    <aside className="w-full min-w-0 shrink-0 md:w-[296px]">
      <OwnerAvatar name={name} kind={kind} size={160} label={handle} />
      <h1 className="mt-4 text-xl font-semibold tracking-tight text-ink">
        {name}
      </h1>
      {secondary && <p className="mt-0.5 text-ink-2">{secondary}</p>}
      {props.bio?.trim() && (
        <p className="mt-4 text-sm text-ink-2">{props.bio}</p>
      )}
      {(props.company?.trim() ||
        props.location?.trim() ||
        links.length > 0) && (
        <ul className="mt-4 flex flex-col gap-1.5">
          {props.company?.trim() && (
            <MetaItem icon={<Building2 aria-hidden className="size-4" />}>
              {props.company}
            </MetaItem>
          )}
          {props.location?.trim() && (
            <MetaItem icon={<MapPin aria-hidden className="size-4" />}>
              {props.location}
            </MetaItem>
          )}
          {links.map((l) => (
            <MetaItem
              key={l.url}
              icon={<LinkIcon aria-hidden className="size-4" />}
            >
              {/* A URL somebody typed about themselves is user-generated
                  content pointed at the open internet: `ugc` and
                  `nofollow` so a profile is not a place to launder
                  ranking, `noopener`/`noreferrer` so the destination
                  gets neither a handle on this tab nor the address of
                  the profile that sent it. */}
              <a
                href={l.href as string}
                rel="nofollow ugc noopener noreferrer"
                className={PROSE_LINK}
              >
                {linkLabel(l)}
              </a>
            </MetaItem>
          ))}
        </ul>
      )}
    </aside>
  );
}
