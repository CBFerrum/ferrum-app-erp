// Bringing a bank statement in: choose the file, check its columns are read the way they are meant,
// see how many lines are new and how many the account has already, and only then import. The check
// is the server's own trial run of the import (nothing is written), so what it reports is what the
// import will do.

import { useQuery } from "@tanstack/react-query";
import { type ChangeEvent, type DragEvent, useMemo, useState } from "react";
import { api, docPath, errorMessage, formatDate, formatDecimal } from "@ferrum/client";
import { Alert, Badge, Button, Dialog, Field, Spinner, toast, useDebounced } from "@ferrum/kit";
import { csvHeaders, DATE_FORMATS, guessMapping, type Mapping, mappingBody, mappingProblem } from "./reconcile";

const MAX_BYTES = 5 << 20;

interface Trial {
  imported: number;
  duplicates: number;
  skipped: number;
  errors: string[];
  from_date?: string | null;
  to_date?: string | null;
  preview: { date: string; description: string; reference: string; deposit: string; withdrawal: string; duplicate: boolean }[];
}

const COLUMNS: { key: keyof Mapping; label: string; hint?: string }[] = [
  { key: "date", label: "Date" },
  { key: "description", label: "Description" },
  { key: "reference", label: "Reference" },
  { key: "amount", label: "Amount", hint: "One column, money out as a minus." },
  { key: "deposit", label: "Money in", hint: "Instead of one amount column." },
  { key: "withdrawal", label: "Money out" },
];

export default function ImportDialog({ account, onClose, onDone }: { account: string; onClose: () => void; onDone: () => void }) {
  const [text, setText] = useState("");
  const [name, setName] = useState("");
  const [pasting, setPasting] = useState(false);
  const [headerRow, setHeaderRow] = useState(1);
  const [mapping, setMapping] = useState<Mapping | null>(null);
  const [dateFormat, setDateFormat] = useState("");
  const [comma, setComma] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const read = useMemo(() => (text ? csvHeaders(text, headerRow) : null), [text, headerRow]);
  const headers = (read?.headers ?? []).filter(Boolean);
  const chosen = mapping ?? (headers.length ? guessMapping(headers) : null);
  const missing = chosen ? mappingProblem(chosen) : null;

  const body = (dry: boolean) => ({
    csv: text,
    mapping: chosen ? mappingBody(chosen) : {},
    header_row: headerRow,
    delimiter: read?.delimiter,
    date_format: dateFormat || undefined,
    decimal_comma: comma,
    dry_run: dry,
  });
  const asked = useDebounced(JSON.stringify(body(true)), 350);
  const trial = useQuery({
    queryKey: ["erp", "bank", "import-trial", account, asked],
    enabled: !!text && !!chosen && !missing,
    queryFn: ({ signal }) => api.post<Trial>(docPath("BankAccount", account, "actions", "import_transactions"), JSON.parse(asked), { signal }),
  });

  const take = (file: File | undefined) => {
    if (!file) return;
    if (file.size > MAX_BYTES) return setProblem("That file is larger than 5 MB. Split the statement and import it in parts.");
    setProblem(null);
    void file.text().then((t) => {
      setText(t);
      setName(file.name);
      setMapping(null);
      setHeaderRow(1);
    });
  };
  const onFile = (e: ChangeEvent<HTMLInputElement>) => take(e.target.files?.[0]);
  const onDrop = (e: DragEvent) => {
    e.preventDefault();
    take(e.dataTransfer.files[0]);
  };

  const run = async () => {
    setBusy(true);
    try {
      const done = await api.post<Trial>(docPath("BankAccount", account, "actions", "import_transactions"), body(false));
      toast(`Imported ${done.imported} ${done.imported === 1 ? "line" : "lines"}`);
      onDone();
    } catch (e) {
      setProblem(errorMessage(e));
      setBusy(false);
    }
  };

  const t = trial.data;
  return (
    <Dialog
      open
      wide
      onClose={onClose}
      title="Import a bank statement"
      footer={
        <>
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" loading={busy} disabled={!t || t.imported === 0 || t.errors.length > 0} onClick={() => void run()}>
            {t && t.imported > 0 ? `Import ${t.imported} ${t.imported === 1 ? "line" : "lines"}` : "Import"}
          </Button>
        </>
      }
    >
      <div className="stack erp-tight">
        {problem && <Alert tone="error">{problem}</Alert>}
        {!text ? (
          <>
            <label className="erp-drop" onDragOver={(e) => e.preventDefault()} onDrop={onDrop}>
              <input type="file" accept=".csv,.txt,text/csv,text/plain" onChange={onFile} data-autofocus />
              <span>Drop the statement here, or choose the file</span>
              <span className="faint">A CSV from your bank, up to 5 MB.</span>
            </label>
            {pasting ? (
              <Field label="The statement's text" htmlFor="imp-text">
                <textarea id="imp-text" className="textarea mono" rows={6} onChange={(e) => setText(e.target.value)} />
              </Field>
            ) : (
              <Button variant="ghost" size="sm" onClick={() => setPasting(true)}>
                Paste the text instead
              </Button>
            )}
          </>
        ) : (
          <>
            <div className="row">
              <strong>{name || "Pasted statement"}</strong>
              <span className="faint">{headers.length} columns</span>
              <Button
                size="sm"
                variant="ghost"
                onClick={() => {
                  setText("");
                  setMapping(null);
                }}
              >
                Choose another
              </Button>
            </div>
            <div className="erp-mapping">
              {COLUMNS.map((c) => (
                <Field key={c.key} label={c.label} htmlFor={`imp-${c.key}`} hint={c.hint}>
                  <select id={`imp-${c.key}`} className="select" value={chosen?.[c.key] ?? ""} onChange={(e) => setMapping({ ...(chosen as Mapping), [c.key]: e.target.value })}>
                    <option value="">—</option>
                    {headers.map((h) => (
                      <option key={h} value={h}>
                        {h}
                      </option>
                    ))}
                  </select>
                </Field>
              ))}
            </div>
            <div className="erp-two">
              <Field label="Dates are written" htmlFor="imp-dates">
                <select id="imp-dates" className="select" value={dateFormat} onChange={(e) => setDateFormat(e.target.value)}>
                  {DATE_FORMATS.map((f) => (
                    <option key={f.value} value={f.value}>
                      {f.label}
                    </option>
                  ))}
                </select>
              </Field>
              <Field label="Header row" htmlFor="imp-header" hint="Which line holds the column names.">
                <input id="imp-header" className="input num" type="number" min={1} max={50} value={headerRow} onChange={(e) => setHeaderRow(Math.max(1, Number(e.target.value) || 1))} />
              </Field>
            </div>
            <label className="check-field">
              <input type="checkbox" checked={comma} onChange={(e) => setComma(e.target.checked)} />
              <span>Amounts use a decimal comma (1.234,50)</span>
            </label>
            {missing ? (
              <Alert tone="warning">{missing}</Alert>
            ) : trial.error ? (
              <Alert tone="error">{errorMessage(trial.error)}</Alert>
            ) : !t ? (
              <Spinner label="Checking the statement" />
            ) : (
              <div className="stack erp-tight" aria-live="polite">
                {t.errors.length > 0 ? (
                  <Alert tone="error">
                    <strong>The statement cannot be imported as it is.</strong>
                    <ul className="erp-problems">
                      {t.errors.slice(0, 8).map((m) => (
                        <li key={m}>{m}</li>
                      ))}
                    </ul>
                    {t.errors.length > 8 && <span className="faint">and {t.errors.length - 8} more.</span>}
                  </Alert>
                ) : (
                  <p>
                    <strong>{t.imported}</strong> new {t.imported === 1 ? "line" : "lines"}
                    {t.from_date && t.to_date ? `, ${formatDate(t.from_date)} to ${formatDate(t.to_date)}` : ""}. {t.duplicates > 0 && <>{t.duplicates} already in the account will be skipped. </>}
                    {t.skipped > 0 && <>{t.skipped} with no amount will be left out.</>}
                  </p>
                )}
                {t.preview.length > 0 && (
                  <div className="table-wrap">
                    <table className="table">
                      <thead>
                        <tr>
                          <th>Date</th>
                          <th>Description</th>
                          <th className="num">In</th>
                          <th className="num">Out</th>
                          <th />
                        </tr>
                      </thead>
                      <tbody>
                        {t.preview.map((l, i) => (
                          <tr key={i} className={l.duplicate ? "erp-skipped" : undefined}>
                            <td className="nowrap">{formatDate(l.date)}</td>
                            <td>
                              {l.description}
                              {l.reference && <span className="faint"> · {l.reference}</span>}
                            </td>
                            <td className="num">{formatDecimal(l.deposit, 2)}</td>
                            <td className="num">{formatDecimal(l.withdrawal, 2)}</td>
                            <td>{l.duplicate && <Badge>Already imported</Badge>}</td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  </div>
                )}
              </div>
            )}
          </>
        )}
      </div>
    </Dialog>
  );
}
