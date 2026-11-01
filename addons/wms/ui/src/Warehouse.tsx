// Which warehouse this is, when there is more than one, at the top of the screens that need it.

import { Link } from "react-router";
import { EmptyState, ErrorState, PageLoading } from "@ferrum/kit";
import { type Managed, useWarehouse } from "./data";

export function InWarehouse({ children }: { children: (w: Managed) => React.ReactNode }) {
  const { list, current, choose, query } = useWarehouse();
  if (query.isPending) return <PageLoading />;
  if (query.isError) return <ErrorState error={query.error} onRetry={() => void query.refetch()} />;
  if (!current) {
    return (
      <EmptyState title="No warehouse is managed here yet" icon="warehouse">
        <Link to="/e/ManagedWarehouse">Set one up</Link>
      </EmptyState>
    );
  }
  return (
    <>
      {list.length > 1 ? (
        <label className="wms-pick">
          Warehouse{" "}
          <select className="select" value={current.warehouse} onChange={(e) => choose(e.target.value)}>
            {list.map((w) => (
              <option key={w.id} value={w.warehouse}>
                {w.name_of_warehouse ?? w.warehouse}
              </option>
            ))}
          </select>
        </label>
      ) : null}
      {children(current)}
    </>
  );
}
