// Which warehouse this is, when there is more than one, at the top of the screens that need it. Every
// screen is the kit's page: a title, one line under it and its actions, then the work in cards.

import type { ReactNode } from "react";
import { Link } from "react-router";
import { LoadFailed, PageHeader, PageLoading, StatePanel } from "@ferrum/kit";
import { type Managed, useWarehouse } from "./data";

/** A screen's frame: the kit's page head above one column of cards. `narrow` is a form for one person at a scanner. */
export function Page({ header, narrow, children }: { header: ReactNode; narrow?: boolean; children?: ReactNode }) {
  return (
    <div className={narrow ? "wms-narrow" : undefined}>
      {header}
      <div className="wms-page">{children}</div>
    </div>
  );
}

export function InWarehouse({ title, description, actions, narrow, children }: { title: string; description?: (w: Managed) => ReactNode; actions?: ReactNode; narrow?: boolean; children: (w: Managed) => ReactNode }) {
  const { list, current, choose, query } = useWarehouse();
  if (query.isPending) {
    return (
      <Page narrow={narrow} header={<PageHeader title={title} />}>
        <PageLoading />
      </Page>
    );
  }
  if (query.isError) {
    return (
      <Page narrow={narrow} header={<PageHeader title={title} />}>
        <LoadFailed error={query.error} onRetry={() => void query.refetch()} />
      </Page>
    );
  }
  if (!current) {
    return (
      <Page narrow={narrow} header={<PageHeader title={title} />}>
        <StatePanel kind="empty" title="No warehouse is managed here yet" action={<Link className="btn btn-primary btn-md" to="/e/ManagedWarehouse">Set one up</Link>}>
          A warehouse has to be set up before its work, stock and bins can be shown.
        </StatePanel>
      </Page>
    );
  }
  return (
    <Page
      narrow={narrow}
      header={
        <PageHeader
          title={title}
          description={description?.(current)}
          actions={
            <>
              {list.length > 1 ? (
                <select className="select wms-pick" aria-label="Warehouse" value={current.warehouse} onChange={(e) => choose(e.target.value)}>
                  {list.map((w) => (
                    <option key={w.id} value={w.warehouse}>
                      {w.name_of_warehouse ?? w.warehouse}
                    </option>
                  ))}
                </select>
              ) : null}
              {actions}
            </>
          }
        />
      }
    >
      {children(current)}
    </Page>
  );
}
