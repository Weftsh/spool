import * as React from "react";
import { cva, type VariantProps } from "class-variance-authority";

import { cn } from "@/lib/utils";

// Boxed messages. role defaults to "alert" for errors; pass
// role="status" for calmer notices the tests expect as status.
const alertVariants = cva("rounded-md border px-3 py-2 text-sm", {
  variants: {
    variant: {
      default: "border-border bg-card text-foreground",
      warning: "border-warning/40 bg-warning/10",
      destructive: "border-serious/40 bg-serious/10",
    },
  },
  defaultVariants: {
    variant: "default",
  },
});

function Alert({
  className,
  variant,
  role = "alert",
  ...props
}: React.ComponentProps<"div"> & VariantProps<typeof alertVariants>) {
  return (
    <div
      data-slot="alert"
      role={role}
      className={cn(alertVariants({ variant, className }))}
      {...props}
    />
  );
}

function AlertTitle({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="alert-title"
      className={cn("mb-1 font-medium", className)}
      {...props}
    />
  );
}

function AlertDescription({
  className,
  ...props
}: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="alert-description"
      className={cn("text-ink-2", className)}
      {...props}
    />
  );
}

export { Alert, AlertTitle, AlertDescription };
