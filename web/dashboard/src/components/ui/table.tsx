import * as React from "react";

import { cn } from "@/lib/utils";

// The house table: header row in muted small-caps, body rows separated
// by hairlines, px-2 py-2 cells. The overflow wrapper is load-bearing —
// a wide table scrolls inside it instead of widening the page (the
// walkthrough fails any horizontal overflow). Real <table>/<tr>/<td>
// only: the e2e suite selects rows and cells by their implicit ARIA
// roles and by positional td locators.
function Table({ className, ...props }: React.ComponentProps<"table">) {
  return (
    <div
      data-slot="table-container"
      className="relative w-full overflow-x-auto"
    >
      <table
        data-slot="table"
        className={cn("w-full text-left", className)}
        {...props}
      />
    </div>
  );
}

function TableHeader({ className, ...props }: React.ComponentProps<"thead">) {
  return (
    <thead data-slot="table-header" className={cn(className)} {...props} />
  );
}

function TableBody({ className, ...props }: React.ComponentProps<"tbody">) {
  return <tbody data-slot="table-body" className={cn(className)} {...props} />;
}

function TableRow({ className, ...props }: React.ComponentProps<"tr">) {
  return (
    <tr
      data-slot="table-row"
      className={cn("border-t border-border text-sm", className)}
      {...props}
    />
  );
}

// The header row resets TableRow's border and carries the small-caps
// look itself, so callers write the same <TableRow> in both sections.
function TableHeadRow({ className, ...props }: React.ComponentProps<"tr">) {
  return (
    <tr
      data-slot="table-head-row"
      className={cn(
        "border-t-0 text-xs uppercase tracking-wider text-muted-foreground",
        className,
      )}
      {...props}
    />
  );
}

function TableHead({ className, ...props }: React.ComponentProps<"th">) {
  return (
    <th
      data-slot="table-head"
      className={cn("px-2 py-2 font-medium", className)}
      {...props}
    />
  );
}

function TableCell({ className, ...props }: React.ComponentProps<"td">) {
  return (
    <td
      data-slot="table-cell"
      className={cn("px-2 py-2", className)}
      {...props}
    />
  );
}

export {
  Table,
  TableHeader,
  TableBody,
  TableRow,
  TableHeadRow,
  TableHead,
  TableCell,
};
