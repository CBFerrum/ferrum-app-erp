// Bringing a bank statement in: choose the file, see how it is read (a statement format that fits it, or the
// columns of a CSV chosen here), how many lines are new and how many the account has already, and only then
// import. The check is the server's own preview of the import (nothing is written), so what it reports is
// what the import will do.

import { useQuery } from "@tanstack/react-query";
import { type ChangeEvent, type DragEvent, useMemo, useState } from "react";
import { ApiError, api, docPath, errorMessage, formatDate, formatDecimal, t as tr, isolate } from "@ferrum/client";
import { Alert, Badge, Button, Dialog, Field, Spinner, toast, useDebounced, placed } from "@ferrum/kit";
import { csvHeaders, DATE_FORMATS, guessMapping, type Mapping, mappingBody, mappingProblem } from "./reconcile";

const MAX_BYTES = 5 << 20;
/** The select's value for a CSV whose columns are chosen in the dialog. */
const BY_COLUMNS = "columns";

export interface Trial {
  format?: { id: string; name: string } | null;
  imported: number;
  duplicates: number;
  skipped: number;
  other_accounts?: number;
  /** After an import: lines automatic rules booked, and those a rule could not book (left to reconcile). */
  ruled?: number;
  rule_failures?: { number?: string | null; rule: string; why: string }[];
  errors: string[];
  from_date?: string | null;
  to_date?: string | null;
  preview: { date: string; description: string; reference: string; deposit: string; withdrawal: string; duplicate: boolean }[];
}

interface Format {
  id: string;
  format_name: string;
  disabled?: boolean | number | null;
}

const COLUMNS: { key: keyof Mapping; label: string; hint?: string }[] = [
  { key: "date", get label() { return tr("Date"); } },
  { key: "description", get label() { return tr("Description"); } },
  { key: "reference", get label() { return tr("Reference"); } },
  { key: "amount", get label() { return tr("Amount"); }, get hint() { return tr("One column, money out as a minus."); } },
  { key: "deposit", get label() { return tr("Money in"); }, get hint() { return tr("Instead of one amount column."); } },
  { key: "withdrawal", get label() { return tr("Money out"); } },
];

const action = (account: string, name: string) => docPath("BankAccount", account, "actions", name);

export default function ImportDialog({ account, onClose, onDone, initial }: { account: string; onClose: () => void; onDone: () => void; initial?: { text: string; name: string } }) {
  const [text, setText] = useState(initial?.text ?? "");
  const [name, setName] = useState(initial?.name ?? "");
  const [pasting, setPasting] = useState(false);
  // "" lets the server choose the format; a format's id, or BY_COLUMNS for columns chosen here.
  const [choice, setChoice] = useState("");
  const [headerRow, setHeaderRow] = useState(1);
  const [mapping, setMapping] = useState<Mapping | null>(null);
  const [dateFormat, setDateFormat] = useState("");
  const [comma, setComma] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [failures, setFailures] = useState<NonNullable<Trial["rule_failures"]>>([]);

  const formats = useQuery({
    queryKey: ["erp", "statement-formats"],
    enabled: !!text,
    queryFn: ({ signal }) => api.get<{ rows: Format[] }>(docPath("StatementFormat"), { fields: "id,format_name,disabled", sort: "format_name asc", limit: 100 }, signal),
  });
  const enabled = (formats.data?.rows ?? []).filter((f) => !f.disabled);

  const fitted = useQuery({
    queryKey: ["erp", "bank", "statement-read", account, choice, text],
    enabled: !!text && choice !== BY_COLUMNS,
    retry: false,
    queryFn: ({ signal }) => api.post<Trial>(action(account, "preview_statement"), { text, format: choice || undefined }, { signal }),
  });
  const byColumns = choice === BY_COLUMNS || fitted.data?.format === null;

  const read = useMemo(() => (text && byColumns ? csvHeaders(text, headerRow) : null), [text, headerRow, byColumns]);
  const headers = (read?.headers ?? []).filter(Boolean);
  const chosen = mapping ?? (headers.length ? guessMapping(headers) : null);
  const missing = byColumns && chosen ? mappingProblem(chosen) : null;
  const inline = JSON.stringify({
    columns: chosen ? mappingBody(chosen) : {},
    header_row: headerRow,
    delimiter: read?.delimiter,
    date_format: dateFormat || undefined,
    decimal_comma: comma,
  });
  const asked = useDebounced(inline, 350);
  const trial = useQuery({
    queryKey: ["erp", "bank", "import-trial", account, asked, text],
    enabled: !!text && byColumns && !!chosen && !missing,
    retry: false,
    queryFn: ({ signal }) => api.post<Trial>(action(account, "preview_statement"), { text, inline: asked }, { signal }),
  });

  const take = (file: File | undefined) => {
    if (!file) return;
    if (file.size > MAX_BYTES) return setProblem(tr("That file is larger than 5 MB. Split the statement and import it in parts."));
    setProblem(null);
    void file.text().then((t) => {
      setText(t);
      setName(file.name);
      setChoice("");
      setMapping(null);
      setHeaderRow(1);
    });
  };
  const onFile = (e: ChangeEvent<HTMLInputElement>) => take(e.target.files?.[0]);
  const onDrop = (e: DragEvent) => {
    e.preventDefault();
    take(e.dataTransfer.files[0]);
  };

  const t = byColumns ? trial.data : fitted.data;
  const failed = byColumns ? trial.error : fitted.error;
  const run = async () => {
    setBusy(true);
    try {
      const body = byColumns ? { text, inline } : { text, format: t?.format?.id };
      const done = await api.post<Trial>(action(account, "import_transactions"), body);
      const said = done.imported === 1 ? tr("Imported {imported} line", { imported: done.imported }) : tr("Imported {imported} lines", { imported: done.imported });
      toast((done.ruled ?? 0) > 0 ? `${said}. ${tr("{ruled} booked by rules", { ruled: done.ruled ?? 0 })}` : said);
      if (done.rule_failures?.length) {
        setFailures(done.rule_failures);
        setBusy(false);
      } else onDone();
    } catch (e) {
      setProblem(errorMessage(e));
      setBusy(false);
    }
  };

  return (
    <Dialog
      open
      wide
      onClose={onClose}
      title={tr("Import a bank statement")}
      footer={
        failures.length > 0 ? (
          <Button variant="primary" onClick={onDone}>
            {tr("Close")}
          </Button>
        ) : (
        <>
          <Button onClick={onClose}>{tr("Cancel")}</Button>
          <Button variant="primary" loading={busy} disabled={!t || t.imported === 0 || t.errors.length > 0} onClick={() => void run()}>
            {t && t.imported > 0 ? (t.imported === 1 ? tr("Import {imported} line", { imported: t.imported }) : tr("Import {imported} lines", { imported: t.imported })) : tr("Import")}
          </Button>
        </>
        )
      }
    >
      <div className="stack erp-tight">
        {problem && <Alert tone="error">{isolate(problem)}</Alert>}
        {failures.length > 0 && (
          <Alert tone="warning">
            <strong>{tr("The statement is imported. These lines were left to reconcile by hand: their rule could not book them.")}</strong>
            <ul className="erp-problems">
              {failures.map((f, i) => (
                <li key={i}>{isolate(`${f.number ?? ""} · ${f.rule}: ${f.why}`)}</li>
              ))}
            </ul>
          </Alert>
        )}
        {!text ? (
          <>
            <label className="erp-drop" onDragOver={(e) => e.preventDefault()} onDrop={onDrop}>
              <input type="file" accept=".csv,.txt,.xml,.sta,.940,.mt940,.ofx,.qfx" onChange={onFile} data-autofocus />
              <span>{tr("Drop the statement here, or choose the file")}</span>
              <span className="faint">{tr("A statement file from your bank, up to 5 MB.")}</span>
            </label>
            {pasting ? (
              <Field label={tr("The statement's text")} htmlFor="imp-text">
                <textarea id="imp-text" className="textarea mono" rows={6} onChange={(e) => setText(e.target.value)} />
              </Field>
            ) : (
              <Button variant="ghost" size="sm" onClick={() => setPasting(true)}>
                {tr("Paste the text instead")}
              </Button>
            )}
          </>
        ) : (
          <>
            <div className="row">
              <strong>{name || tr("Pasted statement")}</strong>
              {byColumns && <span className="faint">{tr("{length} columns", { length: headers.length })}</span>}
              <Button
                size="sm"
                variant="ghost"
                onClick={() => {
                  setText("");
                  setChoice("");
                  setMapping(null);
                }}
              >
                {tr("Choose another")}
              </Button>
            </div>
            {failed instanceof ApiError && failed.status === 403 ? (
              <Alert tone="error">{tr("You may not import statements for this account.")}</Alert>
            ) : !byColumns && !t && !failed ? (
              <Spinner label={tr("Reading the statement…")} />
            ) : (
              <>
                {(t?.format || choice || (byColumns && enabled.length > 0)) && (
                  <Field label={t?.format ? placed(tr("Read as {p1}"), { p1: t.format.name }) : tr("Read as")} htmlFor="imp-format">
                    <select id="imp-format" className="select" value={choice || t?.format?.id || BY_COLUMNS} onChange={(e) => setChoice(e.target.value)}>
                      {t?.format && !enabled.some((f) => f.id === t.format?.id) && <option value={t.format.id}>{isolate(t.format.name)}</option>}
                      {enabled.map((f) => (
                        <option key={f.id} value={f.id}>
                          {isolate(f.format_name)}
                        </option>
                      ))}
                      <option value={BY_COLUMNS}>{tr("CSV, columns chosen here")}</option>
                    </select>
                  </Field>
                )}
                {byColumns && (
                  <>
                    <div className="erp-mapping">
                      {COLUMNS.map((c) => (
                        <Field key={c.key} label={c.label} htmlFor={`imp-${c.key}`} hint={c.hint}>
                          <select id={`imp-${c.key}`} className="select" value={chosen?.[c.key] ?? ""} onChange={(e) => setMapping({ ...(chosen as Mapping), [c.key]: e.target.value })}>
                            <option value="">—</option>
                            {headers.map((h) => (
                              <option key={h} value={h}>
                                {isolate(h)}
                              </option>
                            ))}
                          </select>
                        </Field>
                      ))}
                    </div>
                    <div className="erp-two">
                      <Field label={tr("Dates are written")} htmlFor="imp-dates">
                        <select id="imp-dates" className="select" value={dateFormat} onChange={(e) => setDateFormat(e.target.value)}>
                          {DATE_FORMATS.map((f) => (
                            <option key={f.value} value={f.value}>
                              {isolate(f.label)}
                            </option>
                          ))}
                        </select>
                      </Field>
                      <Field label={tr("Header row")} htmlFor="imp-header" hint={tr("Which line holds the column names.")}>
                        <input id="imp-header" className="input num" type="number" min={1} max={50} value={headerRow} onChange={(e) => setHeaderRow(Math.max(1, Number(e.target.value) || 1))} />
                      </Field>
                    </div>
                    <label className="check-field">
                      <input type="checkbox" checked={comma} onChange={(e) => setComma(e.target.checked)} />
                      <span>{tr("Amounts use a decimal comma (1.234,50)")}</span>
                    </label>
                  </>
                )}
                {missing ? (
                  <Alert tone="warning">{isolate(missing)}</Alert>
                ) : failed ? (
                  <Alert tone="error">{isolate(errorMessage(failed))}</Alert>
                ) : !t ? (
                  <Spinner label={tr("Checking the statement")} />
                ) : (
                  <Outcome t={t} />
                )}
              </>
            )}
          </>
        )}
      </div>
    </Dialog>
  );
}

/** What the preview found: the lines it cannot read, or how many are new, with the first lines as read. */
function Outcome({ t }: { t: Trial }) {
  return (
    <div className="stack erp-tight" aria-live="polite">
      {t.errors.length > 0 ? (
        <Alert tone="error">
          <strong>{tr("The statement cannot be imported as it is.")}</strong>
          <ul className="erp-problems">
            {t.errors.slice(0, 8).map((m) => (
              <li key={m}>{isolate(m)}</li>
            ))}
          </ul>
          {t.errors.length > 8 && <span className="faint">{placed(tr("and {p1} more."), { p1: t.errors.length - 8 })}</span>}
        </Alert>
      ) : (
        <p>
          <strong>{t.imported}</strong> {(t.imported === 1 ? (t.from_date && t.to_date ? tr("new line, {value} to {value2}", { value: formatDate(t.from_date), value2: formatDate(t.to_date) }) : tr("new line")) : (t.from_date && t.to_date ? tr("new lines, {value} to {value2}", { value: formatDate(t.from_date), value2: formatDate(t.to_date) }) : tr("new lines")))}. {t.duplicates > 0 && <>{tr("{duplicates} already in the account will be skipped.", { duplicates: t.duplicates })} </>}
          {t.skipped > 0 && <>{tr("{skipped} with no amount, or not booked, will be left out.", { skipped: t.skipped })} </>}
          {(t.other_accounts ?? 0) > 0 && <>{tr("{other_accounts} statements in the file are another account's and will be left out.", { other_accounts: t.other_accounts ?? 0 })}</>}
        </p>
      )}
      {t.preview.length > 0 && (
        <div className="table-wrap">
          <table className="table">
            <thead>
              <tr>
                <th>{tr("Date")}</th>
                <th>{tr("Description")}</th>
                <th className="num">{tr("In")}</th>
                <th className="num">{tr("Out")}</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {t.preview.map((l, i) => (
                <tr key={i} className={l.duplicate ? "erp-skipped" : undefined}>
                  <td className="nowrap">{isolate(formatDate(l.date))}</td>
                  <td>
                    {isolate(l.description)}
                    {l.reference && <span className="faint"> · {isolate(l.reference)}</span>}
                  </td>
                  <td className="num">{isolate(formatDecimal(l.deposit, 2))}</td>
                  <td className="num">{isolate(formatDecimal(l.withdrawal, 2))}</td>
                  <td>{l.duplicate && <Badge>{tr("Already imported")}</Badge>}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
