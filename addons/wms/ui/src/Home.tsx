// What needs doing here, what is stuck first. Work that needs a person sits at the top and every row
// opens the scanner screen for it.

import { useQuery } from "@tanstack/react-query";
import { Link } from "react-router";
import { formatDecimal } from "@ferrum/client";
import { Badge, EmptyState, ErrorState, Icon, PageLoading, useShellTitle } from "@ferrum/kit";
import { get, type Managed, type Overview, rows, tasks } from "./data";
import { byUrgency, taskWords } from "./logic";
import { InWarehouse } from "./Warehouse";

interface Doc {
  id: string;
  number?: string | null;
  reference?: string | null;
  status?: string | null;
  expected_on?: string | null;
  supplier?: string | null;
  customer?: string | null;
  _titles?: Record<string, string>;
}

function Queue({ title, to, items }: { title: string; to: (d: Doc) => string; items: Doc[] }) {
  if (items.length === 0) return null;
  return (
    <section className="card card-pad">
      <h2>{title}</h2>
      <ul className="wms-list">
        {items.slice(0, 8).map((d) => (
          <li key={d.id}>
            <Link to={to(d)}>
              {d.number || d.reference || d._titles?.customer || d._titles?.supplier || "Open"}
              <span className="faint"> {d._titles?.supplier ?? d._titles?.customer ?? ""}</span>
            </Link>
          </li>
        ))}
      </ul>
    </section>
  );
}

function Floor({ w }: { w: Managed }) {
  const overview = useQuery({ queryKey: ["wms", "overview", w.warehouse], queryFn: () => get<Overview>("/overview", { warehouse: w.warehouse }), refetchInterval: 15_000 });
  const work = useQuery({ queryKey: ["wms", "tasks", w.warehouse], queryFn: () => tasks(w.warehouse), refetchInterval: 15_000 });
  const filter = [["warehouse", "=", w.warehouse]];
  const inbound = useQuery({ queryKey: ["wms", "inbound", w.warehouse], queryFn: () => rows<Doc>("InboundShipment", [...filter, ["status", "in", ["Expected", "Receiving"]]]) });
  const differences = useQuery({ queryKey: ["wms", "differences", w.warehouse], queryFn: () => rows<Doc>("InboundShipment", [...filter, ["status", "=", "Discrepancy"]]) });
  const shipments = useQuery({ queryKey: ["wms", "shipments", w.warehouse], queryFn: () => rows<Doc>("Shipment", [...filter, ["status", "in", ["Picking", "Packed", "Partially Handed Over"]]]) });
  const counts = useQuery({ queryKey: ["wms", "counts", w.warehouse], queryFn: () => rows<Doc>("CycleCount", [...filter, ["status", "in", ["Draft", "Counting", "Review"]]]) });
  if (overview.isPending || work.isPending) return <PageLoading />;
  if (overview.isError) return <ErrorState error={overview.error} onRetry={() => void overview.refetch()} />;
  if (work.isError) return <ErrorState error={work.error} onRetry={() => void work.refetch()} />;
  const o = overview.data;
  const list = byUrgency(work.data);
  const available = Number(o.stock.Available ?? 0);
  const out = Object.entries(o.stock).filter(([s]) => s !== "Available");
  return (
    <>
      <section className="wms-tiles" aria-label="Right now">
        {(["Exception", "Arrived", "Released", "Open", "Moving"] as const).map((s) => (
          <div key={s} className={`card wms-tile${s === "Exception" && o.tasks[s] ? " wms-bad" : ""}`}>
            <strong>{o.tasks[s] ?? 0}</strong>
            <span>{taskWords(s).label}</span>
          </div>
        ))}
      </section>
      {o.blocked_bins || o.closed_zones ? (
        <p className="muted">
          {o.blocked_bins ? `${o.blocked_bins} bins blocked. ` : ""}
          {o.closed_zones ? `${o.closed_zones} zones closed.` : ""}
        </p>
      ) : null}
      <div className="wms-split">
        <section className="card">
          <h2 className="wms-card-title">Work</h2>
          {list.length === 0 ? (
            <EmptyState title="All clear" icon="check">
              Nothing is waiting.
            </EmptyState>
          ) : (
            <ul className="wms-tasks">
              {list.slice(0, 40).map((t) => (
                <li key={t.id}>
                  <Link to={`/t/${t.id}`}>
                    <Badge tone={taskWords(t.status).tone}>{taskWords(t.status).label}</Badge>
                    <span className="wms-task-text">
                      {t.kind} {formatDecimal(t.qty)} {t.item}
                      {t.lot ? ` · ${t.lot}` : ""}
                      <span className="faint">
                        {" "}
                        {t.from_bin ?? "?"} → {t.to_bin ?? "?"}
                      </span>
                    </span>
                    <Icon name="chevronRight" size={14} />
                  </Link>
                </li>
              ))}
            </ul>
          )}
        </section>
        <div className="wms-side">
          <section className="card card-pad">
            <h2>Stock</h2>
            <p>
              <strong>{formatDecimal(available)}</strong> available
            </p>
            {out.map(([s, q]) => (
              <p key={s} className="muted">
                {formatDecimal(Number(q))} {s.toLowerCase()}
              </p>
            ))}
            <Link to="/bin">What is in a bin?</Link>
          </section>
          <Queue title="To receive" to={(d) => `/receive/${d.id}`} items={inbound.data ?? []} />
          <Queue title="Differences to settle" to={(d) => `/receive/${d.id}`} items={differences.data ?? []} />
          <Queue title="To ship" to={(d) => `/ship/${d.id}`} items={shipments.data ?? []} />
          <Queue title="To count" to={(d) => `/count/${d.id}`} items={counts.data ?? []} />
        </div>
      </div>
    </>
  );
}

export function Home() {
  useShellTitle("Warehouse");
  return (
    <div className="wms-page">
      <InWarehouse>{(w) => <Floor w={w} />}</InWarehouse>
    </div>
  );
}
