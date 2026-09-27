import * as React from "react";
import { Slot } from "@radix-ui/react-slot";
import { cva, type VariantProps } from "class-variance-authority";

import { cn } from "@/lib/utils";

// The dashboard's button vocabulary, consolidated from the nine drifted
// copies of the primary classes that used to live inline. Variants map to
// DESIGN.md: primary is emerald with the glow hover, destructive is quiet
// (this product never shouts white-on-red), depth comes from borders, not
// shadows. size="icon" REQUIRES aria-label — the walkthrough fails any
// button with neither text nor a label.
const buttonVariants = cva(
  "inline-flex items-center justify-center gap-2 whitespace-nowrap rounded-md text-sm font-medium transition focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring disabled:pointer-events-none disabled:opacity-60 [&_svg]:pointer-events-none [&_svg]:size-4 [&_svg]:shrink-0",
  {
    variants: {
      variant: {
        default:
          "bg-primary text-primary-foreground hover:bg-brand-strong hover:shadow-[var(--glow-brand)]",
        secondary:
          "border border-border bg-card text-foreground hover:border-ink-3",
        outline: "border border-border text-ink-2 hover:text-foreground",
        ghost: "text-ink-3 hover:text-foreground",
        destructive:
          "border border-border text-ink-2 hover:border-serious/40 hover:text-serious",
        link: "text-primary underline-offset-4 hover:underline",
      },
      size: {
        // The dashboard's dominant density (px-3 py-1.5) is the default;
        // lg is the full-width auth submit.
        default: "px-3 py-1.5",
        lg: "px-4 py-2",
        xs: "px-2.5 py-1 text-xs",
        icon: "size-8",
      },
    },
    defaultVariants: {
      variant: "default",
      size: "default",
    },
  },
);

function Button({
  className,
  variant,
  size,
  asChild = false,
  ...props
}: React.ComponentProps<"button"> &
  VariantProps<typeof buttonVariants> & { asChild?: boolean }) {
  const Comp = asChild ? Slot : "button";
  return (
    <Comp
      data-slot="button"
      className={cn(buttonVariants({ variant, size, className }))}
      {...props}
    />
  );
}

export { Button, buttonVariants };
