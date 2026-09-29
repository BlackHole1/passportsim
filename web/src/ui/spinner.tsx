// Vendored from coss ui (https://github.com/cosscom/coss, apps/ui/registry/default/ui/spinner.tsx at 8423f18a), MIT.
// Only the import paths are changed; see THIRD_PARTY.md.
import { Loader2Icon } from "lucide-react";
import type React from "react";
import { cn } from "./lib/utils";

export function Spinner({
  className,
  ...props
}: React.ComponentProps<typeof Loader2Icon>): React.ReactElement {
  return (
    <Loader2Icon
      aria-label="Loading"
      className={cn("animate-spin", className)}
      role="status"
      {...props}
    />
  );
}
