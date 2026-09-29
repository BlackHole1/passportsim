// Vendored from coss ui (https://github.com/cosscom/coss, apps/ui/registry/default/lib/utils.ts at 8423f18a), MIT.
// See THIRD_PARTY.md.
import { type ClassValue, clsx } from "clsx";
import { twMerge } from "tailwind-merge";

export function cn(...inputs: ClassValue[]): string {
  return twMerge(clsx(inputs));
}
