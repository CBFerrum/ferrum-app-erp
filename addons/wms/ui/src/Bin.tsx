// What is in a bin. The answer to most questions on a warehouse floor, one scan away.

import { useQuery } from "@tanstack/react-query";
import { useSearchParams } from "react-router";
import { formatDecimal } from "@ferrum/client";
import { LoadFailed, PageLoading, StatePanel, StatusBadge } from "@ferrum/kit";
import { BIN_TONES, STOCK_TONES } from "./logic";
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
  if (q.isError) return <LoadFailed error={q.error} onRetry={() => void q.refetch()} />;
  const b = q.data;
  return (
    <section className="card">
      <div className="card-head">
        <h2>{b.bin}</h2>
        {b.status === "Blocked" ? <StatusBadge value={b.status} tones={BIN_TONES} label="Blocked" /> : null}
        {b.frozen ? <StatusBadge value="Frozen" tones={{ Frozen: "amber" }} label="Being counted" /> : null}
      </div>
      {b.positions.length === 0 ? (
        <StatePanel kind="empty" className="pt-4 md:pt-4 pb-6" title="Empty">
          Nothing is in this bin.
        </StatePanel>
      ) : (
        <ul className="wms-list">
          {b.positions.map((p) => (
            <li key={p.id} className="wms-line">
              <span>
                {p.item_code} {p.lot ? <span className="faint">{p.lot}</span> : null} {p.status !== "Available" ? <StatusBadge value={p.status} tones={STOCK_TONES} /> : null}
              </span>
              <span className="num">
                {Number(p.allocated) > 0 ? <span className="faint">{formatDecimal(p.allocated)} promised · </span> : null}
                {formatDecimal(p.qty)}
              </span>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

export function BinLookup() {
  const [params, setParams] = useSearchParams();
  const code = params.get("code");
  return (
    <InWarehouse title="What is in a bin" narrow description={(w) => w.name_of_warehouse ?? w.warehouse}>
      {(w) => (
        <>
          <ScanBox label="Scan a bin" onScan={(c) => setParams({ code: c })} />
          {code ? <Contents w={w} code={code} /> : null}
        </>
      )}
    </InWarehouse>
  );
}
