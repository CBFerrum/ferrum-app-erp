// Following a lot: forward from a material to everything made with it, for a recall, and back from a
// product to everything in it, for an investigation.

import { useQuery } from "@tanstack/react-query";
import { useId, useState } from "react";
import { formatDecimal } from "@ferrum/client";
import { Badge, Button, EmptyState, ErrorState, Field, PageLoading, useShellTitle } from "@ferrum/kit";
import { LinkInput } from "@ferrum/records";
import { get, type TraceStep } from "./data";
import { Tabs } from "./Home";

export function Trace() {
  useShellTitle("Trace a lot");
  const box = useId();
  const [item, setItem] = useState<string | null>(null);
  const [lot, setLot] = useState("");
  const [asked, setAsked] = useState<{ item: string; lot: string; forward: boolean } | null>(null);
  const q = useQuery({
    queryKey: ["mes", "trace", asked],
    enabled: !!asked,
    queryFn: () => get<{ steps: TraceStep[] }>(`/trace/${asked!.forward ? "forward" : "back"}`, { item: asked!.item, lot: asked!.lot }).then((r) => r.steps),
  });
  const ask = (forward: boolean) => item && lot && setAsked({ item, lot, forward });
  return (
    <div className="mes-page">
      <Tabs />
      <section className="card card-pad mes-filters">
        <Field label="Item" htmlFor={`${box}-i`}>
          <LinkInput to="Item" id={`${box}-i`} value={item} onChange={setItem} />
        </Field>
        <Field label="Lot" htmlFor={`${box}-l`}>
          <input id={`${box}-l`} className="input" autoComplete="off" value={lot} onChange={(e) => setLot(e.target.value.trim())} />
        </Field>
        <div className="mes-trace-actions">
          <Button variant="primary" disabled={!item || !lot} onClick={() => ask(true)}>
            What was made with it
          </Button>
          <Button disabled={!item || !lot} onClick={() => ask(false)}>
            What went into it
          </Button>
        </div>
      </section>
      {!asked ? null : q.isPending ? (
        <PageLoading />
      ) : q.isError ? (
        <ErrorState error={q.error} onRetry={() => void q.refetch()} />
      ) : q.data.length === 0 ? (
        <EmptyState title="No record of that lot in production" icon="search" />
      ) : (
        <ul className="card mes-list">
          {q.data.map((s, i) => (
            <li key={i} className="mes-row" style={{ paddingLeft: `${s.depth}rem` }}>
              <span>
                {formatDecimal(s.qty)} {s.item} {s.lot} → {s.scrapped ? <Badge tone="amber">Scrapped</Badge> : `${s.into_item} ${s.into_lot}`}
              </span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
