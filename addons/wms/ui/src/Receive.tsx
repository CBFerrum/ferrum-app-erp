// Receiving at the dock. Scan an item, say how many, mark what is damaged. Scans can be repeated
// safely: a scanner that lost its answer may send one twice and it counts once.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useId, useRef, useState } from "react";
import { Link, useParams } from "react-router";
import { api, docPath, errorMessage, formatDecimal, newKey } from "@ferrum/client";
import { Alert, Badge, Button, Dialog, ErrorState, Field, PageLoading, toast, useShellTitle } from "@ferrum/kit";
import { post } from "./data";
import { lineProgress, type ReceiveLine } from "./logic";
import { ScanBox } from "./Scan";

interface Inbound {
  id: string;
  number?: string | null;
  status: string;
  reference?: string | null;
  items: ReceiveLine[];
  _titles?: Record<string, string>;
  notes?: string | null;
}

function Unknown({ id, onClose }: { id: string; onClose: () => void }) {
  const qc = useQueryClient();
  const box = useId();
  const [description, setDescription] = useState("");
  const [qty, setQty] = useState("1");
  const send = useMutation({
    mutationFn: () => post(`/inbound/${id}/unknown`, { description, qty }),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["wms"] });
      onClose();
    },
    onError: (e) => toast(errorMessage(e), "error"),
  });
  return (
    <Dialog
      open
      title="Something nobody expected"
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>Back</Button>
          <Button variant="primary" disabled={!description.trim()} loading={send.isPending} onClick={() => send.mutate()}>
            Note it
          </Button>
        </>
      }
    >
      <div className="stack">
        <p className="muted">It stays out of stock until someone says what it is.</p>
        <Field label="What it looks like" htmlFor={`${box}-d`} required>
          <input id={`${box}-d`} className="input" value={description} onChange={(e) => setDescription(e.target.value)} />
        </Field>
        <Field label="How many" htmlFor={`${box}-q`}>
          <input id={`${box}-q`} className="input" inputMode="decimal" value={qty} onChange={(e) => setQty(e.target.value)} />
        </Field>
      </div>
    </Dialog>
  );
}

export function Receive() {
  const { id = "" } = useParams();
  const qc = useQueryClient();
  const q = useQuery({ queryKey: ["wms", "inbound-doc", id], queryFn: () => api.get<Inbound>(docPath("InboundShipment", id), { titles: true }) });
  useShellTitle("Receiving");
  const [qty, setQty] = useState("1");
  const [damaged, setDamaged] = useState(false);
  const [unknown, setUnknown] = useState(false);
  const qtyId = useId();
  const pending = useRef<string | null>(null);
  const scan = useMutation({
    mutationFn: (line: ReceiveLine) => {
      pending.current ??= newKey();
      return post(`/inbound/${id}/scan`, { scan_id: pending.current, line: line.id, qty: damaged ? 0 : qty, damaged: damaged ? qty : 0 });
    },
    onSuccess: () => {
      pending.current = null;
      setDamaged(false);
      void qc.invalidateQueries({ queryKey: ["wms", "inbound-doc", id] });
    },
    onError: (e) => toast(errorMessage(e), "error"),
  });
  const close = useMutation({
    mutationFn: () => post<{ status: string }>(`/inbound/${id}/close`),
    onSuccess: (r) => {
      toast(r.status === "Received" ? "Received in full" : "Received, with a difference to look at");
      void qc.invalidateQueries({ queryKey: ["wms"] });
    },
    onError: (e) => toast(errorMessage(e), "error"),
  });
  if (q.isPending) return <PageLoading />;
  if (q.isError) return <ErrorState error={q.error} onRetry={() => void q.refetch()} />;
  const doc = q.data;
  const open = doc.status === "Expected" || doc.status === "Receiving";
  const find = (code: string) => doc.items.find((l) => (l._titles?.item ?? "").toLowerCase().includes(code.toLowerCase()) || l.lot === code);
  return (
    <div className="wms-page wms-narrow">
      <Link to="/" className="faint">
        Back to the floor
      </Link>
      <h1 className="wms-title">{doc.number ?? "Inbound"}</h1>
      <div>
        <Badge>{doc.status}</Badge> {doc.reference ? <span className="faint">Their reference {doc.reference}</span> : null}
      </div>
      {doc.notes ? <Alert tone="warning">{doc.notes}</Alert> : null}
      {open ? (
        <section className="card card-pad stack">
          <Field label="How many" htmlFor={qtyId}>
            <input id={qtyId} className="input" inputMode="decimal" value={qty} onChange={(e) => setQty(e.target.value)} />
          </Field>
          <label className="wms-check">
            <input type="checkbox" checked={damaged} onChange={(e) => setDamaged(e.target.checked)} /> These are damaged
          </label>
          <ScanBox
            label="Scan the item"
            busy={scan.isPending}
            onScan={(code) => {
              const line = find(code);
              if (line) scan.mutate(line);
              else toast(`${code} is not on this shipment`, "error");
            }}
          />
        </section>
      ) : null}
      <ul className="wms-list card card-pad">
        {doc.items.map((l) => {
          const p = lineProgress(l);
          return (
            <li key={l.id} className="wms-line">
              <span>
                {l._titles?.item ?? l.item} {l.lot ? <span className="faint">{l.lot}</span> : null}
              </span>
              <span>
                {formatDecimal(p.got)} of {formatDecimal(l.expected_qty)} {p.over ? <Badge tone="amber">Over</Badge> : null}
                {Number(l.damaged_qty ?? 0) > 0 ? <Badge tone="red">{formatDecimal(l.damaged_qty ?? 0)} damaged</Badge> : null}
              </span>
            </li>
          );
        })}
      </ul>
      {open ? (
        <div className="wms-actions">
          <Button onClick={() => setUnknown(true)}>Something else came</Button>
          <Button variant="primary" loading={close.isPending} onClick={() => close.mutate()}>
            Finish receiving
          </Button>
        </div>
      ) : null}
      {unknown ? <Unknown id={id} onClose={() => setUnknown(false)} /> : null}
    </div>
  );
}
