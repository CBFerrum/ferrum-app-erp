// What needs doing here, what is stuck first. Work that needs a person sits at the top and every row
// opens the scanner screen for it.

import { useQuery, type UseQueryResult } from "@tanstack/react-query";
import { CheckIcon } from "lucide-react";
import { Link } from "react-router";
import { formatDecimal } from "@ferrum/client";
import { Badge, Icon, LoadFailed, PageLoading, StatePanel } from "@ferrum/kit";
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

/** One kind of document waiting for a person. A list that failed says so: "nothing to receive" would be a claim. */
function Queue({ title, to, query }: { title: string; to: (d: Doc) => string; query: UseQueryResult<Doc[]> }) {
  if (query.isError) {
    return (
      <section className="card">
        <div className="card-head">
          <h2>{title}</h2>
        </div>
        <StatePanel kind="failed" className="pt-4 md:pt-4 pb-6" title="This list could not be loaded" error={query.error} onRetry={() => void query.refetch()} />
      </section>
    );
  }
  const items = query.data ?? [];
  if (items.length === 0) return null;
  return (
    <section className="card">
      <div className="card-head">
        <h2>{title}</h2>
      </div>
      <ul className="wms-rows">
        {items.slice(0, 8).map((d) => (
          <li key={d.id}>
            <Link to={to(d)}>
              <span className="wms-task-text">{d.number || d.reference || d._titles?.customer || d._titles?.supplier || "Open"}</span>
              <span className="faint">{d._titles?.supplier ?? d._titles?.customer ?? ""}</span>
              <Icon name="chevronRight" size={14} />
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
  if (overview.isError) return <LoadFailed error={overview.error} onRetry={() => void overview.refetch()} />;
  if (work.isError) return <LoadFailed error={work.error} onRetry={() => void work.refetch()} />;
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
          <div className="card-head">
            <h2>Work</h2>
          </div>
          {list.length === 0 ? (
            <StatePanel kind="empty" className="pt-4 md:pt-4 pb-6" icon={CheckIcon} title="All clear">
              Nothing is waiting.
            </StatePanel>
          ) : (
            <ul className="wms-rows">
              {list.slice(0, 40).map((t) => (
                <li key={t.id}>
                  <Link to={`/t/${t.id}`}>
                    <Badge tone={taskWords(t.status).tone}>{taskWords(t.status).label}</Badge>
                    <span className="wms-task-text">
                      {t.kind} {t.item}
                      {t.lot ? ` · ${t.lot}` : ""}
                      <span className="faint wms-route-line">
                        {t.from_bin ?? "?"} → {t.to_bin ?? "?"}
                      </span>
                    </span>
                    <span className="num">{formatDecimal(t.qty)}</span>
                    <Icon name="chevronRight" size={14} />
                  </Link>
                </li>
              ))}
            </ul>
          )}
        </section>
        <div className="wms-side">
          <section className="card">
            <div className="card-head">
              <h2>Stock</h2>
            </div>
            <dl className="wms-figures">
              <div>
                <dt>Available</dt>
                <dd>{formatDecimal(available)}</dd>
              </div>
              {out.map(([s, q]) => (
                <div key={s}>
                  <dt className="muted">{s}</dt>
                  <dd className="muted">{formatDecimal(Number(q))}</dd>
                </div>
              ))}
            </dl>
          </section>
          <Queue title="To receive" to={(d) => `/receive/${d.id}`} query={inbound} />
          <Queue title="Differences to settle" to={(d) => `/receive/${d.id}`} query={differences} />
          <Queue title="To ship" to={(d) => `/ship/${d.id}`} query={shipments} />
          <Queue title="To count" to={(d) => `/count/${d.id}`} query={counts} />
        </div>
      </div>
    </>
  );
}

export function Home() {
  return (
    <InWarehouse
      title="Warehouse"
      description={(w) => w.name_of_warehouse ?? w.warehouse}
      actions={
        <Link className="btn btn-primary btn-md" to="/bin">
          What is in a bin?
        </Link>
      }
    >
      {(w) => <Floor w={w} />}
    </InWarehouse>
  );
}
