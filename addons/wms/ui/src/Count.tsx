// Counting. In a blind count the counter sees where to look and not what to expect. A difference is
// counted twice, and someone else approves it. Each figure is saved as it is entered.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Link, useParams } from "react-router";
import { api, docPath, errorMessage, formatDecimal } from "@ferrum/client";
import { Alert, Badge, Button, ErrorState, PageLoading, toast, useShellTitle } from "@ferrum/kit";
import { post } from "./data";

interface Line {
  id: string;
  bin: string;
  item: string;
  lot?: string | null;
  expected_qty?: string | null;
  counted_qty?: string | null;
  recount_qty?: string | null;
  variance?: string | null;
  _titles?: Record<string, string>;
}

interface CountDoc {
  id: string;
  reference?: string | null;
  status: string;
  blind?: boolean;
  lines: Line[];
  _titles?: Record<string, string>;
}

function Row({ id, line, recount }: { id: string; line: Line; recount: boolean }) {
  const qc = useQueryClient();
  const [value, setValue] = useState("");
  const send = useMutation({
    mutationFn: () => post(`/counts/${id}/enter`, { line: line.id, qty: value, recount }),
    onSuccess: () => {
      setValue("");
      void qc.invalidateQueries({ queryKey: ["wms", "count", id] });
    },
    onError: (e) => toast(errorMessage(e), "error"),
  });
  const counted = recount ? line.recount_qty : line.counted_qty;
  return (
    <li className="wms-line">
      <span>
        <strong>{line._titles?.bin ?? line.bin}</strong> {line._titles?.item ?? line.item} {line.lot ? <span className="faint">{line.lot}</span> : null}
      </span>
      <form
        className="wms-count"
        onSubmit={(e) => {
          e.preventDefault();
          if (value !== "") send.mutate();
        }}
      >
        {counted !== null && counted !== undefined ? <Badge tone="green">{formatDecimal(counted)}</Badge> : null}
        <input className="input" inputMode="decimal" aria-label={`Count of ${line._titles?.item ?? "this line"}`} value={value} onChange={(e) => setValue(e.target.value)} />
        <Button type="submit" size="sm" loading={send.isPending}>
          Save
        </Button>
      </form>
    </li>
  );
}

export function Count() {
  const { id = "" } = useParams();
  const qc = useQueryClient();
  const q = useQuery({ queryKey: ["wms", "count", id], queryFn: () => api.get<CountDoc>(docPath("CycleCount", id), { titles: true }) });
  useShellTitle("Count");
  const step = (verb: string) => () => post(`/counts/${id}/${verb}`);
  const after = {
    onSuccess: () => void qc.invalidateQueries({ queryKey: ["wms"] }),
    onError: (e: unknown) => toast(errorMessage(e), "error"),
  };
  const start = useMutation({ mutationFn: step("start"), ...after });
  const finish = useMutation({ mutationFn: step("finish"), ...after });
  const approve = useMutation({ mutationFn: step("approve"), ...after });
  const [recount, setRecount] = useState(false);
  if (q.isPending) return <PageLoading />;
  if (q.isError) return <ErrorState error={q.error} onRetry={() => void q.refetch()} />;
  const c = q.data;
  return (
    <div className="wms-page wms-narrow">
      <Link to="/" className="faint">
        Back to the floor
      </Link>
      <h1 className="wms-title">{c.reference ?? "Count"}</h1>
      <div>
        <Badge>{c.status}</Badge> {c.blind ? <span className="faint">Blind: you will not be told what to expect</span> : null}
      </div>
      {c.status === "Draft" ? (
        <section className="card card-pad stack">
          <p className="muted">Starting freezes the bins: nothing goes in or out until the count is applied.</p>
          <div>
            <Button variant="primary" loading={start.isPending} onClick={() => start.mutate()}>
              Start counting
            </Button>
          </div>
        </section>
      ) : null}
      {c.status === "Counting" ? (
        <>
          <label className="wms-check">
            <input type="checkbox" checked={recount} onChange={(e) => setRecount(e.target.checked)} /> This is the second count
          </label>
          <ul className="wms-list card card-pad">
            {c.lines.map((l) => (
              <Row key={l.id} id={id} line={l} recount={recount} />
            ))}
          </ul>
          <div>
            <Button variant="primary" loading={finish.isPending} onClick={() => finish.mutate()}>
              Finished counting
            </Button>
          </div>
        </>
      ) : null}
      {c.status === "Review" ? (
        <section className="card card-pad stack">
          <h2>Differences</h2>
          <ul className="wms-list">
            {c.lines
              .filter((l) => Number(l.variance ?? 0) !== 0)
              .map((l) => (
                <li key={l.id} className="wms-line">
                  <span>
                    {l._titles?.bin ?? l.bin} {l._titles?.item ?? l.item}
                  </span>
                  <span>{formatDecimal(l.variance ?? 0)}</span>
                </li>
              ))}
          </ul>
          <Alert tone="info">Someone other than the counter approves this. ERP's stock follows.</Alert>
          <div>
            <Button variant="primary" loading={approve.isPending} onClick={() => approve.mutate()}>
              Approve and apply
            </Button>
          </div>
        </section>
      ) : null}
      {c.status === "Applied" ? <p className="muted">Applied. The bins are open again.</p> : null}
    </div>
  );
}
