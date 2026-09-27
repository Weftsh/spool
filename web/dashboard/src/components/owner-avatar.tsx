import { Avatar, AvatarFallback, AvatarImage } from "@/components/ui/avatar";
import { cn } from "@/lib/utils";

/// Who an actor is, in one square of chrome.
///
/// An agent principal never borrows a human's look: it gets square
/// corners, which is GitHub's own convention for a bot and therefore
/// costs a migrating maintainer nothing to learn. The marker is
/// neutral, not pejorative — a project that welcomes agents and one that
/// refuses them both need to *see* which is which (FORGE-UX §6).
export type OwnerKind = "user" | "org" | "agent";

/// Two letters at most: an avatar full of initials is a wall of text at
/// 20px, which is the size this renders at in a list row.
export function initials(name: string): string {
  const words = name.trim().split(/\s+/).filter(Boolean);
  if (words.length === 0) return "?";
  if (words.length === 1) return words[0].slice(0, 2).toUpperCase();
  return (words[0][0] + words[words.length - 1][0]).toUpperCase();
}

export function OwnerAvatar(props: {
  name: string;
  kind?: OwnerKind;
  src?: string | null;
  /// Pixels. Sized inline rather than by class because the spec calls
  /// for 20px in an identity row and 160px on a profile, and a Tailwind
  /// class cannot be built from a variable.
  size?: number;
  /// Give this only when the avatar stands alone. Everywhere the spec
  /// puts one, the handle is rendered next to it — so by default the
  /// image is decorative and a screen reader is not told the name
  /// twice.
  label?: string;
  className?: string;
}) {
  const size = props.size ?? 20;
  const kind = props.kind ?? "user";
  return (
    <Avatar
      style={{ width: size, height: size }}
      className={cn(
        "border border-borderline",
        kind === "agent" ? "rounded" : "rounded-full",
        props.className,
      )}
      {...(props.label
        ? { role: "img", "aria-label": props.label }
        : { "aria-hidden": true })}
    >
      {props.src ? <AvatarImage src={props.src} alt="" /> : null}
      <AvatarFallback
        className={kind === "agent" ? "rounded" : "rounded-full"}
        style={{ fontSize: Math.max(9, Math.round(size * 0.4)) }}
      >
        {initials(props.name)}
      </AvatarFallback>
    </Avatar>
  );
}
