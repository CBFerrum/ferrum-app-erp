// What is in a bin. The answer to most questions on a warehouse floor, one scan away.

import { useQuery } from "@tanstack/react-query";
import { useSearchParams } from "react-router";
import { formatDecimal } from "@ferrum/client";
import { Badge, EmptyState, ErrorState, PageLoading, useShellTitle } from "@ferrum/kit";
import { get, type Managed } from "./data";
import { ScanBox } from "./Scan";
import { InWarehouse } from "./Warehouse";

interface Position {
  id: string;
  item_code: string;
  item_name: string;
  lot: string;
  status: string;
  expiry: string | null;
  qty: string;
  allocated: string;
}

function Contents({ w, code }: { w: Managed; code: string }) {
  const q = useQuery({ queryKey: ["wms", "bin", code], queryFn: () => get<{ bin: string; status: string; frozen: boolean; positions: Position[] }>(`/bins/${encodeURIComponent(code)}`, { warehouse: w.warehouse }) });
  if (q.isPending) return <PageLoading />;
  if (q.isError) return <ErrorState error={q.error} onRetry={() => void q.refetch()} />;
  const b = q.data;
  return (
    <section className="card card-pad stack">
      <h2>
        {b.bin} {b.status === "Blocked" ? <Badge tone="red">Blocked</Badge> : null} {b.frozen ? <Badge tone="amber">Being counted</Badge> : null}
      </h2>
      {b.positions.length === 0 ? (
        <EmptyState title="Empty" icon="list" />
      ) : (
        <ul className="wms-list">
          {b.positions.map((p) => (
            <li key={p.id} className="wms-line">
              <span>
                {p.item_code} {p.lot ? <span className="faint">{p.lot}</span> : null} {p.status !== "Available" ? <Badge tone="amber">{p.status}</Badge> : null}
              </span>
              <span>
                {formatDecimal(p.qty)}
                {Number(p.allocated) > 0 ? <span className="faint"> ({formatDecimal(p.allocated)} promised)</span> : null}
              </span>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

export function BinLookup() {
  useShellTitle("What is in a bin");
  const [params, setParams] = useSearchParams();
  const code = params.get("code");
  return (
    <div className="wms-page wms-narrow">
      <InWarehouse>
        {(w) => (
          <>
            <ScanBox label="Scan a bin" onScan={(c) => setParams({ code: c })} />
            {code ? <Contents w={w} code={code} /> : null}
          </>
        )}
      </InWarehouse>
    </div>
  );
}
