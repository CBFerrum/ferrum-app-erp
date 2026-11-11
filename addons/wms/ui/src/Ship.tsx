// Packing and the door. Every unit is scanned before it can leave, and the label must match. Only
// what was verified goes: a partial shipment says it is one.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useId, useState } from "react";
import { Link, useParams } from "react-router";
import { api, docPath, formatDecimal, refusal } from "@ferrum/client";
import { Alert, Button, Field, LoadFailed, PageHeader, PageLoading, StatusBadge, toast } from "@ferrum/kit";
import { post } from "./data";
import { readyToGo, SHIPMENT_TONES, type ShipLine, unverified } from "./logic";
import { ScanBox } from "./Scan";
import { Page } from "./Warehouse";

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
  const box = useId();
  const blank = { carrier: "", tracking: "", label: "", weighed_kg: "" };
  const [form, setForm] = useState(blank);
  const [qty, setQty] = useState("1");
  const verify = useMutation({
    mutationFn: (code: string) => post(`/shipments/${id}/verify`, { code, qty }),
    onSuccess: () => {
      setQty("1");
      void qc.invalidateQueries({ queryKey: ["wms", "shipment", id] });
    },
    onError: (e) => toast(refusal(e), "error"),
  });
  const hand = useMutation({
    mutationFn: () => post<{ whole: boolean }>(`/shipments/${id}/hand-over`, { ...form, weighed_kg: form.weighed_kg || null }),
    onSuccess: (r) => {
      toast(r.whole ? "Handed over" : "Handed over in part: the rest stays on the shipment");
      setForm(blank);
      void qc.invalidateQueries({ queryKey: ["wms"] });
    },
    onError: (e) => toast(refusal(e), "error"),
  });
  if (q.isPending) return <PageLoading />;
  if (q.isError) return <LoadFailed error={q.error} onRetry={() => void q.refetch()} />;
  const s = q.data;
  const left = unverified(s.items);
  const ready = readyToGo(s.items);
  const done = s.status === "Handed Over" || s.status === "Cancelled";
  const set = (k: keyof typeof form) => (e: { target: { value: string } }) => setForm({ ...form, [k]: e.target.value });
  return (
    <Page narrow header={<PageHeader eyebrow={<Link to="/">Back to the floor</Link>} title={s.reference ?? "Shipment"} description={<StatusBadge value={s.status} tones={SHIPMENT_TONES} />} />}>
      {!done && left.length > 0 ? (
        <section className="card card-pad stack">
          <Field label="How many" htmlFor={`${box}-q`}>
            <input id={`${box}-q`} className="input" inputMode="decimal" value={qty} onChange={(e) => setQty(e.target.value)} />
          </Field>
          <ScanBox label="Scan the item" onScan={(code) => verify.mutate(code)} busy={verify.isPending} />
        </section>
      ) : null}
      <ul className="wms-list card">
        {s.items.map((l) => (
          <li key={l.id} className="wms-line">
            <span>
              {l._titles?.item ?? l.item} {l.lot ? <span className="faint">{l.lot}</span> : null}
            </span>
            <span className="wms-line-end">
              {Number(l.handed_over_qty ?? 0) > 0 ? <span className="faint">{formatDecimal(l.handed_over_qty ?? 0)} gone</span> : null}
              <span className="num">
                {formatDecimal(l.verified_qty ?? 0)} of {formatDecimal(l.qty)}
              </span>
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
          <h2 className="text-base font-semibold">The door</h2>
          {left.length > 0 && ready.length > 0 ? <Alert tone="warning">Not everything is verified. Only what is verified will go.</Alert> : null}
          {ready.length === 0 ? <p className="muted">Nothing is verified and waiting to go.</p> : null}
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
            <Button variant="primary" loading={hand.isPending} disabled={ready.length === 0 || !form.carrier || !form.tracking || !form.label} onClick={() => hand.mutate()}>
              Hand it over
            </Button>
          </div>
        </section>
      )}
    </Page>
  );
}
