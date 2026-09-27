import * as React from "react";

import { cn } from "@/lib/utils";

// A plain <label>: htmlFor/id association is a test contract here
// (getByLabel throughout the e2e suite), so nothing may get between the
// attribute and the element.
function Label({ className, ...props }: React.ComponentProps<"label">) {
  return (
    <label
      data-slot="label"
      className={cn("mb-1 block text-sm font-medium", className)}
      {...props}
    />
  );
}

export { Label };
