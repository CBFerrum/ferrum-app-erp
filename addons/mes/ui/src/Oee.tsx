// OEE for one workstation and period, with the numbers behind it, and honest about what was not seen.
// It is an indicator: one machine's OEE is not the line's, and chasing it can hurt the line.

import { useQuery } from "@tanstack/react-query";
import { useId, useState } from "react";
import { Alert, EmptyState, ErrorState, Field, PageLoading, useShellTitle } from "@ferrum/kit";
import { LinkInput } from "@ferrum/records";
import { get, type OeeAnswer } from "./data";
import { Tabs } from "./Home";
import { coverageNote, cycleWarning, percent } from "./logic";

const toLocal = (d: Date) => new Date(d.getTime() - d.getTimezoneOffset() * 60_000).toISOString().slice(0, 16);
const hours = (secs: number) => (secs / 3600).toFixed(1);

export function Oee() {
  useShellTitle("OEE");
  const box = useId();
  const [station, setStation] = useState<string | null>(null);
  const [from, setFrom] = useState(() => toLocal(new Date(Date.now() - 8 * 3600_000)));
  const [to, setTo] = useState(() => toLocal(new Date()));
  const q = useQuery({
    queryKey: ["mes", "oee", station, from, to],
    enabled: !!station,
    queryFn: () => get<OeeAnswer>("/oee", { workstation: station!, from: new Date(from).toISOString(), to: new Date(to).toISOString() }),
  });
  const f = q.data?.figures;
  const notes = [coverageNote(f?.coverage), cycleWarning(f?.performance)].filter(Boolean);
  return (
    <div className="mes-page">
      <Tabs />
      <section className="card card-pad mes-filters">
        <Field label="Workstation" htmlFor={`${box}-w`}>
          <LinkInput to="Workstation" id={`${box}-w`} value={station} onChange={setStation} />
        </Field>
        <Field label="From" htmlFor={`${box}-f`}>
          <input id={`${box}-f`} className="input" type="datetime-local" value={from} onChange={(e) => setFrom(e.target.value)} />
        </Field>
        <Field label="To" htmlFor={`${box}-t`}>
          <input id={`${box}-t`} className="input" type="datetime-local" value={to} onChange={(e) => setTo(e.target.value)} />
        </Field>
      </section>
      {!station ? (
        <EmptyState title="Choose a workstation" icon="activity" />
      ) : q.isPending ? (
        <PageLoading />
      ) : q.isError ? (
        <ErrorState error={q.error} onRetry={() => void q.refetch()} />
      ) : (
        <>
          <section className="mes-figures" aria-label="Figures">
            {[
              ["OEE", f?.oee],
              ["Availability", f?.availability],
              ["Performance", f?.performance],
              ["Quality", f?.quality],
            ].map(([label, v]) => (
              <div key={label as string} className="card card-pad">
                <span className="faint">{label as string}</span>
                <strong>{percent(v as number | null)}</strong>
              </div>
            ))}
          </section>
          {notes.map((n) => (
            <Alert key={n} tone="warning">
              {n}
            </Alert>
          ))}
          <p className="muted">
            {hours(q.data.input.run_secs)} h running of {hours(q.data.input.planned_secs)} h planned, {q.data.input.good_count} good of {q.data.input.total_count} made. Definition version {f?.version}.
          </p>
        </>
      )}
    </div>
  );
}
