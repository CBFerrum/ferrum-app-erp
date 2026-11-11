// The states a page can be in when it has nothing to show, told apart (kit docs/design/PATTERNS.md):
// loading is shaped like what loads, a refusal is not a failure, and a lost connection is not a fault of the page.

import { ApiError, t } from "@ferrum/client";
import { Button, FormSkeleton, ReaderSkeleton, StatePanel, TableSkeleton } from "@ferrum/kit";

/** Why a load failed, as the state the person sees: the server said no (denied), no answer came (offline), or it failed. */
export function LoadFailed({ error, onRetry, title }: { error: unknown; onRetry?: () => void; title?: string }) {
  const status = error instanceof ApiError ? error.status : undefined;
  if (status === 403) return <StatePanel kind="denied" />;
  if (status === 0) return (
      <StatePanel kind="offline" action={onRetry && <Button icon="refresh" onClick={onRetry}>{t("Try again")}</Button>}>
        {(error as ApiError).message}
      </StatePanel>
    );
  return <StatePanel kind="failed" title={title} error={error} onRetry={onRetry} />;
}

/**
 * A skeleton shaped like the page's content while it loads; `label` (what is being waited for) is kept for
 * assistive technology, as the spinner had it.
 */
export function Loading({ label, shape = "table", columns = 4, rows = 5 }: { label?: string; shape?: "table" | "reader" | "form"; columns?: number; rows?: number }) {
  return (
    <>
      {shape === "reader" ? <ReaderSkeleton /> : shape === "form" ? <FormSkeleton /> : <TableSkeleton columns={columns} rows={rows} />}
      {label && <span className="sr-only">{label}</span>}
    </>
  );
}
