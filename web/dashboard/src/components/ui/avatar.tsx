import * as React from "react";
import { Avatar as AvatarPrimitive } from "radix-ui";

import { cn } from "@/lib/utils";

// Radix's avatar, kept to its three real parts. The registry also ships
// AvatarBadge/AvatarGroup/AvatarGroupCount; they are not wired to
// anything yet, and COMPONENTS.md's rule is that an unused primitive is
// drift waiting to happen — the About panel's contributor grid can
// re-add the group when it lands.
//
// Shape is deliberately not fixed here: a human owner is `rounded-full`
// and an agent principal is `rounded` (FORGE-UX §6 adopts GitHub's own
// bot convention), so OwnerAvatar passes the radius in.
function Avatar({
  className,
  ...props
}: React.ComponentProps<typeof AvatarPrimitive.Root>) {
  return (
    <AvatarPrimitive.Root
      data-slot="avatar"
      className={cn(
        "relative flex size-8 shrink-0 overflow-hidden rounded-full select-none",
        className,
      )}
      {...props}
    />
  );
}

function AvatarImage({
  className,
  ...props
}: React.ComponentProps<typeof AvatarPrimitive.Image>) {
  return (
    <AvatarPrimitive.Image
      data-slot="avatar-image"
      className={cn("aspect-square size-full", className)}
      {...props}
    />
  );
}

function AvatarFallback({
  className,
  ...props
}: React.ComponentProps<typeof AvatarPrimitive.Fallback>) {
  return (
    <AvatarPrimitive.Fallback
      data-slot="avatar-fallback"
      className={cn(
        "flex size-full items-center justify-center bg-secondary text-xs font-medium text-muted-foreground",
        className,
      )}
      {...props}
    />
  );
}

export { Avatar, AvatarImage, AvatarFallback };
