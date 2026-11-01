// The terminal for one run. Big buttons, one screen, usable with gloves on. Every press is an event
// with its own id, so a press sent twice by a shaky connection still counts once.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useId, useState } from "react";
import { Link, useParams } from "react-router";
import { api, docPath, formatDateTime, formatDecimal, newKey, refusal } from "@ferrum/client";
import { Alert, Badge, Button, confirmDialog, Dialog, ErrorState, Field, PageLoading, toast, useShellTitle } from "@ferrum/kit";
import { LinkInput } from "@ferrum/records";
import { get, post, type Run, rows } from "./data";
import { remaining, runWords } from "./logic";

interface EventRow {
  seq: number;
  kind: string;
  at: string;
  actor: string | null;
  lot: string;
  qty: string | null;
  scrap: string | null;
  rework: string | null;
  detail: { reason?: string; note?: string } | null;
}

function useStep(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (step: Record<string, unknown>) => post(`/runs/${id}/events`, { event_id: newKey(), ...step }),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ["mes"] }),
    onError: (e) => toast(refusal(e), "error"),
  });
}

/** Why the last press was refused, inside the dialog: a toast sits behind one. */
function Refused({ step }: { step: ReturnType<typeof useStep> }) {
  return step.isError ? <Alert tone="error">{refusal(step.error)}</Alert> : null;
}

function Pause({ id, onClose }: { id: string; onClose: () => void }) {
  const step = useStep(id);
  const q = useQuery({ queryKey: ["mes", "reasons"], queryFn: () => rows<{ id: string; reason: string; category: string }>("DowntimeReason", [], { limit: 100 }) });
  return (
    <Dialog open title="Why has it stopped?" onClose={onClose}>
      <Refused step={step} />
      {q.isPending ? (
        <PageLoading />
      ) : q.isError ? (
        <ErrorState error={q.error} />
      ) : (
        <div className="mes-reasons">
          {q.data.map((r) => (
            <Button key={r.id} onClick={() => step.mutate({ kind: "pause", reason: r.reason }, { onSuccess: onClose })}>
              {r.reason}
            </Button>
          ))}
        </div>
      )}
    </Dialog>
  );
}

function Consume({ id, onClose }: { id: string; onClose: () => void }) {
  const step = useStep(id);
  const box = useId();
  const [item, setItem] = useState<string | null>(null);
  const [lot, setLot] = useState("");
  const [qty, setQty] = useState("");
  return (
    <Dialog
      open
      title="Material going in"
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>Back</Button>
          <Button variant="primary" disabled={!item || !qty} loading={step.isPending} onClick={() => step.mutate({ kind: "consume", item, lot, qty }, { onSuccess: onClose })}>
            Record it
          </Button>
        </>
      }
    >
      <div className="stack">
        <Refused step={step} />
        <Field label="Material" htmlFor={`${box}-i`} required>
          <LinkInput to="Item" id={`${box}-i`} value={item} required onChange={setItem} />
        </Field>
        <Field label="Lot" htmlFor={`${box}-l`} hint="Scan the label.">
          <input id={`${box}-l`} className="input" autoComplete="off" value={lot} onChange={(e) => setLot(e.target.value.trim())} />
        </Field>
        <Field label="How much" htmlFor={`${box}-q`} required>
          <input id={`${box}-q`} className="input" inputMode="decimal" value={qty} onChange={(e) => setQty(e.target.value)} />
        </Field>
      </div>
    </Dialog>
  );
}

function Report({ id, onClose }: { id: string; onClose: () => void }) {
  const step = useStep(id);
  const box = useId();
  const [good, setGood] = useState("");
  const [scrap, setScrap] = useState("");
  const [rework, setRework] = useState("");
  const [lot, setLot] = useState("");
  const send = () => step.mutate({ kind: "report", good: good || 0, scrap: scrap || 0, rework: rework || 0, lot }, { onSuccess: onClose });
  return (
    <Dialog
      open
      title="What came off"
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>Back</Button>
          <Button variant="primary" disabled={!good && !scrap && !rework} loading={step.isPending} onClick={send}>
            Report it
          </Button>
        </>
      }
    >
      <div className="stack">
        <Refused step={step} />
        <Field label="Good" htmlFor={`${box}-g`}>
          <input id={`${box}-g`} className="input" inputMode="decimal" value={good} onChange={(e) => setGood(e.target.value)} />
        </Field>
        <Field label="Scrap" htmlFor={`${box}-s`}>
          <input id={`${box}-s`} className="input" inputMode="decimal" value={scrap} onChange={(e) => setScrap(e.target.value)} />
        </Field>
        <Field label="To rework" htmlFor={`${box}-r`}>
          <input id={`${box}-r`} className="input" inputMode="decimal" value={rework} onChange={(e) => setRework(e.target.value)} />
        </Field>
        <Field label="Lot made" htmlFor={`${box}-l`}>
          <input id={`${box}-l`} className="input" autoComplete="off" value={lot} onChange={(e) => setLot(e.target.value.trim())} />
        </Field>
      </div>
    </Dialog>
  );
}

function Complete({ id, short, onClose }: { id: string; short: boolean; onClose: () => void }) {
  const step = useStep(id);
  const box = useId();
  const [note, setNote] = useState("");
  return (
    <Dialog
      open
      title="Finish this run"
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>Back</Button>
          <Button variant="primary" disabled={short && !note.trim()} loading={step.isPending} onClick={() => step.mutate({ kind: "complete", note }, { onSuccess: onClose })}>
            Finish
          </Button>
        </>
      }
    >
      <div className="stack">
        <Refused step={step} />
        <Field label={short ? "It is short of its quantity. Why?" : "Anything to note"} htmlFor={`${box}-n`} required={short}>
          <textarea id={`${box}-n`} className="textarea" rows={2} value={note} onChange={(e) => setNote(e.target.value)} />
        </Field>
      </div>
    </Dialog>
  );
}

const EVENT_WORDS: Record<string, string> = { Release: "Released", Start: "Started", Pause: "Stopped", Resume: "Resumed", Consume: "Material in", Report: "Reported", Complete: "Finished", Cancel: "Cancelled" };

export function RunPage() {
  const { id = "" } = useParams();
  const q = useQuery({ queryKey: ["mes", "run", id], queryFn: () => api.get<Run>(docPath("ProductionRun", id), { titles: true }), refetchInterval: 10_000 });
  const events = useQuery({ queryKey: ["mes", "events", id], queryFn: () => get<{ events: EventRow[] }>(`/runs/${id}/events`).then((r) => r.events), refetchInterval: 10_000 });
  const step = useStep(id);
  const [open, setOpen] = useState<"pause" | "consume" | "report" | "complete" | null>(null);
  useShellTitle(q.data?._titles?.workstation ?? "Run");
  if (q.isPending) return <PageLoading />;
  if (q.isError) return <ErrorState error={q.error} onRetry={() => void q.refetch()} />;
  const r = q.data;
  const word = runWords(r.status);
  const left = remaining(r.qty, r.good_qty, r.scrap_qty);
  const live = r.status === "Running" || r.status === "Paused";
  return (
    <div className="mes-page mes-narrow">
      <Link to="/" className="faint">
        Back to the floor
      </Link>
      <header>
        <h1 className="mes-title">{r._titles?.workstation ?? "Run"}</h1>
        <Badge tone={word.tone}>{word.label}</Badge> <span className="faint">{r.number} · {r._titles?.work_order ?? ""}</span>
      </header>
      {r.note ? <Alert tone="warning">{r.note}</Alert> : null}
      <dl className="mes-counts">
        <div>
          <dt>Good</dt>
          <dd>{formatDecimal(r.good_qty ?? 0)}</dd>
        </div>
        <div>
          <dt>Scrap</dt>
          <dd>{formatDecimal(r.scrap_qty ?? 0)}</dd>
        </div>
        <div>
          <dt>Rework</dt>
          <dd>{formatDecimal(r.rework_qty ?? 0)}</dd>
        </div>
        <div>
          <dt>To go</dt>
          <dd>{formatDecimal(left)}</dd>
        </div>
      </dl>
      <div className="mes-buttons">
        {r.status === "Released" ? (
          <Button variant="primary" loading={step.isPending} onClick={() => step.mutate({ kind: "start" })}>
            Start
          </Button>
        ) : null}
        {r.status === "Running" ? <Button onClick={() => setOpen("pause")}>It stopped</Button> : null}
        {r.status === "Paused" ? (
          <Button variant="primary" loading={step.isPending} onClick={() => step.mutate({ kind: "resume" })}>
            Running again
          </Button>
        ) : null}
        {live ? <Button onClick={() => setOpen("consume")}>Material in</Button> : null}
        {live ? (
          <Button variant="primary" onClick={() => setOpen("report")}>
            Report
          </Button>
        ) : null}
        {live ? <Button onClick={() => setOpen("complete")}>Finish</Button> : null}
        {r.status === "Planned" || r.status === "Released" ? (
          <Button
            onClick={async () => {
              if (await confirmDialog({ title: "Cancel this run?", message: "Anything sent to the machine can only be asked to stop.", confirmLabel: "Cancel the run" })) step.mutate({ kind: "cancel", reason: "Cancelled at the terminal" });
            }}
          >
            Cancel the run
          </Button>
        ) : null}
      </div>
      <section className="card card-pad">
        <h2>What happened</h2>
        {events.isPending ? (
          <PageLoading />
        ) : events.isError ? (
          <ErrorState error={events.error} onRetry={() => void events.refetch()} />
        ) : (
          <ol className="mes-story">
            {[...events.data].reverse().map((e) => (
              <li key={e.seq}>
                <strong>{EVENT_WORDS[e.kind] ?? e.kind}</strong> <span className="faint">{formatDateTime(e.at)}</span>
                <div className="muted">{[e.detail?.reason, e.lot, e.qty ? `${formatDecimal(e.qty)}` : null, e.scrap && Number(e.scrap) ? `${formatDecimal(e.scrap)} scrap` : null, e.detail?.note].filter(Boolean).join(" · ")}</div>
              </li>
            ))}
          </ol>
        )}
      </section>
      {open === "pause" ? <Pause id={id} onClose={() => setOpen(null)} /> : null}
      {open === "consume" ? <Consume id={id} onClose={() => setOpen(null)} /> : null}
      {open === "report" ? <Report id={id} onClose={() => setOpen(null)} /> : null}
      {open === "complete" ? <Complete id={id} short={left > 0} onClose={() => setOpen(null)} /> : null}
    </div>
  );
}
