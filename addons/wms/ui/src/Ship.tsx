// Packing and the door. Every unit is scanned before it can leave, and the label must match. Only
// what was verified goes: a partial shipment says it is one.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useId, useState } from "react";
import { Link, useParams } from "react-router";
import { api, docPath, errorMessage, formatDecimal } from "@ferrum/client";
import { Alert, Badge, Button, ErrorState, Field, PageLoading, toast, useShellTitle } from "@ferrum/kit";
import { post } from "./data";
import { type ShipLine, unverified } from "./logic";
import { ScanBox } from "./Scan";

interface ShipmentDoc {
  id: string;
  reference?: string | null;
  status: string;
  carrier?: string | null;
  tracking?: string | null;
  expected_weight_kg?: string | null;
  items: ShipLine[];
  _titles?: Record<string, string>;
}

export function Ship() {
  const { id = "" } = useParams();
  const qc = useQueryClient();
  const q = useQuery({ queryKey: ["wms", "shipment", id], queryFn: () => api.get<ShipmentDoc>(docPath("Shipment", id), { titles: true }) });
  useShellTitle("Packing");
  const box = useId();
  const [form, setForm] = useState({ carrier: "", tracking: "", label: "", weighed_kg: "" });
  const verify = useMutation({
    mutationFn: (item_code: string) => post(`/shipments/${id}/verify`, { item_code, qty: 1 }),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ["wms", "shipment", id] }),
    onError: (e) => toast(errorMessage(e), "error"),
  });
  const hand = useMutation({
    mutationFn: () => post<{ whole: boolean }>(`/shipments/${id}/hand-over`, { ...form, weighed_kg: form.weighed_kg || null }),
    onSuccess: (r) => {
      toast(r.whole ? "Handed over" : "Handed over in part: the rest stays on the shipment");
      void qc.invalidateQueries({ queryKey: ["wms"] });
    },
    onError: (e) => toast(errorMessage(e), "error"),
  });
  if (q.isPending) return <PageLoading />;
  if (q.isError) return <ErrorState error={q.error} onRetry={() => void q.refetch()} />;
  const s = q.data;
  const left = unverified(s.items);
  const done = s.status === "Handed Over" || s.status === "Cancelled";
  const set = (k: keyof typeof form) => (e: { target: { value: string } }) => setForm({ ...form, [k]: e.target.value });
  return (
    <div className="wms-page wms-narrow">
      <Link to="/" className="faint">
        Back to the floor
      </Link>
      <h1 className="wms-title">{s.reference ?? "Shipment"}</h1>
      <Badge>{s.status}</Badge>
      {!done && left.length > 0 ? <ScanBox label="Scan each unit" onScan={(code) => verify.mutate(code)} busy={verify.isPending} /> : null}
      <ul className="wms-list card card-pad">
        {s.items.map((l) => (
          <li key={l.id} className="wms-line">
            <span>
              {l._titles?.item ?? l.item} {l.lot ? <span className="faint">{l.lot}</span> : null}
            </span>
            <span>
              {formatDecimal(l.verified_qty ?? 0)} of {formatDecimal(l.qty)}
            </span>
          </li>
        ))}
      </ul>
      {done ? (
        s.tracking ? (
          <p className="muted">
            {s.carrier} {s.tracking}
          </p>
        ) : null
      ) : (
        <section className="card card-pad stack">
          <h2>The door</h2>
          {left.length > 0 ? <Alert tone="warning">Not everything is verified. Only what is verified will go.</Alert> : null}
          <Field label="Carrier" htmlFor={`${box}-c`} required>
            <input id={`${box}-c`} className="input" value={form.carrier} onChange={set("carrier")} />
          </Field>
          <Field label="Tracking number" htmlFor={`${box}-t`} required>
            <input id={`${box}-t`} className="input" value={form.tracking} onChange={set("tracking")} />
          </Field>
          <Field label="Scan the label on the parcel" htmlFor={`${box}-l`} required>
            <input id={`${box}-l`} className="input" autoComplete="off" value={form.label} onChange={set("label")} />
          </Field>
          <Field label="Weight (kg)" htmlFor={`${box}-w`} hint={s.expected_weight_kg ? `It should weigh about ${formatDecimal(s.expected_weight_kg)}` : undefined}>
            <input id={`${box}-w`} className="input" inputMode="decimal" value={form.weighed_kg} onChange={set("weighed_kg")} />
          </Field>
          <div>
            <Button variant="primary" loading={hand.isPending} disabled={!form.carrier || !form.tracking || !form.label} onClick={() => hand.mutate()}>
              Hand it over
            </Button>
          </div>
        </section>
      )}
    </div>
  );
}
