import * as React from "react";
import { cva, type VariantProps } from "class-variance-authority";

import { cn } from "@/lib/utils";

// Status pills. Color never carries meaning alone here — pair the badge
// with a glyph or plain words (SyncBadge is the reference), per
// DESIGN.md.
const badgeVariants = cva(
  "inline-flex items-center gap-1 rounded-full bg-secondary px-2.5 py-1 text-xs font-medium",
  {
    variants: {
      variant: {
        neutral: "text-ink-2",
        good: "text-good",
        warning: "text-warning",
        serious: "text-serious",
      },
    },
    defaultVariants: {
      variant: "neutral",
    },
  },
);

function Badge({
  className,
  variant,
  ...props
}: React.ComponentProps<"span"> & VariantProps<typeof badgeVariants>) {
  return (
    <span
      data-slot="badge"
      className={cn(badgeVariants({ variant, className }))}
      {...props}
    />
  );
}

export { Badge, badgeVariants };
