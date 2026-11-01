// One task, on a handheld. Scan where it is, scan where it went, and the server says yes or why not.
// A robot's task asks only for the destination: arriving is not delivering. When nobody can say where
// the stock is, this is where someone says.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useId, useState } from "react";
import { Link, useParams } from "react-router";
import { api, docPath, formatDecimal, newKey, refusal } from "@ferrum/client";
import { Alert, Badge, Button, confirmDialog, Dialog, ErrorState, Field, PageLoading, toast, useShellTitle } from "@ferrum/kit";
import { LinkInput } from "@ferrum/records";
import { post } from "./data";
import { custodyWords, shortOf, taskWords } from "./logic";
import { ScanBox } from "./Scan";

interface TaskDoc {
  id: string;
  kind: string;
  status: string;
  summary: string;
  qty: string;
  done_qty?: string | null;
  lot?: string | null;
  custody?: string | null;
  robot?: string | null;
  reason?: string | null;
  command?: string | null;
  from_bin?: string | null;
  to_bin?: string | null;
  _titles?: Record<string, string>;
}

function Recover({ t, onClose }: { t: TaskDoc; onClose: () => void }) {
  const id = useId();
  const qc = useQueryClient();
  const [bin, setBin] = useState("");
  const [qty, setQty] = useState(t.qty);
  const [note, setNote] = useState("");
  const send = useMutation({
    mutationFn: () => post(`/tasks/${t.id}/recover`, { found_bin: bin.trim(), qty, note }),
    onSuccess: () => {
      toast("Recorded");
      void qc.invalidateQueries({ queryKey: ["wms"] });
      onClose();
    },
    onError: (e) => toast(refusal(e), "error"),
  });
  return (
    <Dialog
      open
      title="Where is it?"
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>Not yet</Button>
          <Button variant="primary" disabled={!bin.trim() || !note.trim()} loading={send.isPending} onClick={() => send.mutate()}>
            Record it
          </Button>
        </>
      }
    >
      <div className="stack">
        <Field label="The bin it is in" htmlFor={`${id}-bin`} required hint="If it never left, scan the bin it started in.">
          <input id={`${id}-bin`} className="input" value={bin} onChange={(e) => setBin(e.target.value)} />
        </Field>
        <Field label="How much you found" htmlFor={`${id}-qty`} required>
          <input id={`${id}-qty`} className="input" inputMode="decimal" value={qty} onChange={(e) => setQty(e.target.value)} />
        </Field>
        <Field label="What you found" htmlFor={`${id}-note`} required>
          <textarea id={`${id}-note`} className="textarea" rows={2} value={note} onChange={(e) => setNote(e.target.value)} />
        </Field>
      </div>
    </Dialog>
  );
}

function Release({ t }: { t: TaskDoc }) {
  const id = useId();
  const qc = useQueryClient();
  const [robot, setRobot] = useState<string | null>(null);
  const send = useMutation({
    mutationFn: () => post(`/tasks/${t.id}/release`, { robot }),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ["wms"] }),
    onError: (e) => toast(refusal(e), "error"),
  });
  return (
    <section className="card card-pad stack">
      <h2>Who does this?</h2>
      <Field label="Robot" htmlFor={`${id}-robot`} hint="Leave empty for a person.">
        <LinkInput to="Device" id={`${id}-robot`} value={robot} linkFilters={{ kind: "Robot" }} onChange={setRobot} />
      </Field>
      <div>
        <Button variant="primary" loading={send.isPending} onClick={() => send.mutate()}>
          {robot ? "Send the robot" : "Release it"}
        </Button>
      </div>
    </section>
  );
}

function Confirm({ t }: { t: TaskDoc }) {
  const qc = useQueryClient();
  const robot = !!t.robot;
  const [from, setFrom] = useState<string | null>(null);
  const [qty, setQty] = useState(t.qty);
  const [reason, setReason] = useState("");
  const id = useId();
  const short = shortOf(t.qty, qty);
  const send = useMutation({
    mutationFn: (to: string) => post(`/tasks/${t.id}/confirm`, { from_scan: from, to_scan: to, qty, reason: short ? reason : null, scan_id: newKey() }),
    onSuccess: () => {
      toast("Done");
      void qc.invalidateQueries({ queryKey: ["wms"] });
    },
    onError: (e) => toast(refusal(e), "error"),
  });
  const needFrom = !robot && from === null;
  return (
    <section className="card card-pad stack">
      {needFrom ? (
        <ScanBox label={`Scan where it is: ${t._titles?.from_bin ?? ""}`} onScan={setFrom} />
      ) : (
        <>
          {!robot ? (
            <p className="muted">
              Took it from {from}.{" "}
              <button type="button" className="link" onClick={() => setFrom(null)}>
                Not right?
              </button>
            </p>
          ) : null}
          <Field label="How much" htmlFor={`${id}-qty`}>
            <input id={`${id}-qty`} className="input" inputMode="decimal" value={qty} onChange={(e) => setQty(e.target.value)} />
          </Field>
          {short ? (
            <Field label="Why less" htmlFor={`${id}-why`} required>
              <input id={`${id}-why`} className="input" value={reason} onChange={(e) => setReason(e.target.value)} />
            </Field>
          ) : null}
          <ScanBox label={`Scan where it went: ${t._titles?.to_bin ?? ""}`} onScan={(code) => send.mutate(code)} busy={send.isPending} />
        </>
      )}
    </section>
  );
}

export function TaskPage() {
  const { id = "" } = useParams();
  const qc = useQueryClient();
  const q = useQuery({ queryKey: ["wms", "task", id], queryFn: () => api.get<TaskDoc>(docPath("WarehouseTask", id), { titles: true }), refetchInterval: 10_000 });
  useShellTitle("Task");
  const [recovering, setRecovering] = useState(false);
  const cancel = useMutation({
    mutationFn: () => post<{ status: string }>(`/tasks/${id}/cancel`, { reason: "Cancelled by a manager" }),
    onSuccess: (r) => {
      toast(r.status === "Cancelled" ? "Cancelled" : "Asked to stop: it stays as it is until the robot answers.");
      void qc.invalidateQueries({ queryKey: ["wms"] });
    },
    onError: (e) => toast(refusal(e), "error"),
  });
  if (q.isPending) return <PageLoading />;
  if (q.isError) return <ErrorState error={q.error} onRetry={() => void q.refetch()} />;
  const t = q.data;
  const word = taskWords(t.status);
  const open = ["Open", "Released", "Moving"].includes(t.status);
  const custody = custodyWords(t.custody ?? null, t.status);
  return (
    <div className="wms-page wms-narrow">
      <Link to="/" className="faint">
        Back to the floor
      </Link>
      <header>
        <h1 className="wms-title">
          {t.kind} {formatDecimal(t.qty)} {t._titles?.item ?? ""}
        </h1>
        {t.lot ? <div className="faint">Lot {t.lot}</div> : null}
        <div className="wms-route">
          <strong>{t._titles?.from_bin ?? "?"}</strong> → <strong>{t._titles?.to_bin ?? "somewhere to choose"}</strong>
        </div>
        <Badge tone={word.tone}>{word.label}</Badge> <span className="muted">{word.says}</span>
      </header>
      {custody ? <Alert tone={t.custody === "Uncertain" ? "error" : "info"}>{custody}</Alert> : null}
      {t.reason ? <p className="muted">{t.reason}</p> : null}
      {t.status === "Open" && t.to_bin ? <Release t={t} /> : null}
      {t.status === "Open" && !t.to_bin ? <Alert tone="warning">No bin fits this. A manager has to choose one.</Alert> : null}
      {(t.status === "Open" && t.to_bin && !t.robot) || (t.status === "Released" && !t.command) || t.status === "Arrived" ? <Confirm t={t} /> : null}
      {t.status === "Exception" ? (
        <Button variant="primary" onClick={() => setRecovering(true)}>
          I found it
        </Button>
      ) : null}
      {open ? (
        <div>
          <Button
            loading={cancel.isPending}
            onClick={async () => {
              if (await confirmDialog({ title: "Cancel this task?", message: "A robot on its way can only be asked to stop.", confirmLabel: "Cancel the task" })) cancel.mutate();
            }}
          >
            Cancel the task
          </Button>
        </div>
      ) : null}
      {recovering ? <Recover t={t} onClose={() => setRecovering(false)} /> : null}
    </div>
  );
}
