// The tax return workbench (src/tax_return.rs). Choose a company, a return definition and the first
// day of a period, and the server works the return's boxes out from the tax ledger; each box that is
// a sum drills down to the documents behind it; a box can be adjusted by hand with a reason; the
// return can be exported in any file format a template gives for it (a country's own formats are
// its add-on's templates); and filing it locks the period against invoices. Boxes, formulas, rates,
// names and formats are the definition's and the templates': nothing of a country is in this screen.

import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Link, useNavigate, useParams, useSearchParams } from "react-router";
import { api, docPath, download, errorMessage, formatDate, isolate, newKey, t } from "@ferrum/client";
import { Alert, Badge, Button, confirmDialog, StatePanel, toast, useShellTitle } from "@ferrum/kit";
import { LinkInput, useMe } from "@ferrum/records";
import { Ctl, Meta, PageHead } from "./chrome";
import { amountInput, Gate, useRights } from "./desk";
import { Loading, LoadFailed } from "./states";
import { show } from "./money";
import { type Adjust, adjustBody, boxOf, type Definition, drillable, type Drill, edited, kindOf, needReason, startBody, status, type TaxReturn as Return } from "./taxReturns";

const KIND_WORD = (k: string | undefined) => (k === "Sum" ? t("From the tax ledger") : k === "Formula" ? t("Worked out from other boxes") : k === "By Hand" ? t("Entered by hand") : "");
const STATUS_WORD = { draft: () => t("Draft"), filed: () => t("Filed"), cancelled: () => t("Cancelled") };
const STATUS_TONE = { draft: "amber", filed: "green", cancelled: "neutral" } as const;

/** `/desk/tax-return` and `/e/TaxReturn/new`: start a return. */
export function TaxReturnStart() {
  useShellTitle(t("Tax return"));
  return (
    <Gate entity="TaxReturn">
      <Start />
    </Gate>
  );
}

/** `/desk/tax-return/:id`: a return's boxes, the documents behind them, adjustments, files and filing. */
export function TaxReturnDesk() {
  const { id = "" } = useParams();
  useShellTitle(t("Tax return"));
  return (
    <Gate entity="TaxReturn">
      <Desk id={id} />
    </Gate>
  );
}

interface Row {
  id: string;
  number?: string | null;
  from_date: string;
  to_date?: string | null;
  docstatus: 0 | 1 | 2;
  filed_on?: string | null;
}

function Start() {
  const navigate = useNavigate();
  const me = useMe().data;
  const may = useRights("TaxReturn").may;
  const [sp, setSp] = useSearchParams();
  const company = sp.get("company") ?? me?.default_company ?? null;
  const definition = sp.get("definition");
  const from = sp.get("from") ?? "";
  const put = (k: string, v: string | null) => setSp((p) => { const n = new URLSearchParams(p); v ? n.set(k, v) : n.delete(k); return n; }, { replace: true });
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const [key, setKey] = useState(newKey);
  const earlier = useQuery({
    queryKey: ["erp", "returns", company, definition],
    enabled: !!company && !!definition,
    queryFn: ({ signal }) => api.get<{ rows: Row[] }>(docPath("TaxReturn"), { filters: JSON.stringify({ company, definition }), fields: "id,number,from_date,to_date,docstatus,filed_on", sort: "from_date desc", limit: 24 }, signal),
  });

  const start = async () => {
    if (!company || !definition || !from || busy) return;
    setBusy(true);
    setProblem(null);
    try {
      const made = await api.post<{ id: string }>(docPath("TaxReturn"), startBody({ definition, company, from }), { idempotencyKey: key });
      navigate(`/desk/tax-return/${made.id}`);
    } catch (e) {
      setProblem(errorMessage(e));
      setKey(newKey());
      setBusy(false);
    }
  };

  return (
    <div className="erp-page">
      <PageHead title={t("Tax return")} meta={<Meta items={[t("Choose a return and the first day of its period. The boxes are worked out from the books.")]} />} actions={<Link className="btn btn-ghost btn-md" to="/e/TaxReturn">{t("Earlier returns")}</Link>} />
      <form className="erp-bar" onSubmit={(e) => e.preventDefault()} aria-label={t("Which return")}>
        <Ctl label={t("Company")} htmlFor="tr-company">
          <LinkInput to="Company" id="tr-company" value={company} onChange={(id) => put("company", id)} />
        </Ctl>
        <Ctl label={t("Return")} htmlFor="tr-def">
          <LinkInput to="TaxReturnDefinition" id="tr-def" value={definition} onChange={(id) => put("definition", id)} />
        </Ctl>
        <Ctl label={t("Period starts")} htmlFor="tr-from">
          <input id="tr-from" className="input" type="date" value={from} onChange={(e) => put("from", e.target.value || null)} />
        </Ctl>
      </form>
      {problem && <Alert tone="error">{isolate(problem)}</Alert>}
      {earlier.data && earlier.data.rows.length > 0 && (
        <section className="erp-panel" aria-labelledby="tr-earlier">
          <h2 id="tr-earlier" className="erp-card-title">{t("Returns made for this definition")}</h2>
          <ul className="erp-list">
            {earlier.data.rows.map((r) => {
              const s = status(r);
              return (
                <li key={r.id}>
                  <Link to={`/desk/tax-return/${r.id}`}>{isolate(r.number ?? "")} {formatDate(r.from_date)}{r.to_date ? ` – ${formatDate(r.to_date)}` : ""}</Link> <Badge tone={STATUS_TONE[s]}>{STATUS_WORD[s]()}</Badge>
                </li>
              );
            })}
          </ul>
        </section>
      )}
      {earlier.error && <LoadFailed error={earlier.error} onRetry={() => void earlier.refetch()} title={t("Could not load the earlier returns")} />}
      <div className="erp-sticky-foot">
        <span className="muted">{may("create") ? "" : t("You can look at tax returns. An Accounts Manager makes one.")}</span>
        <Button variant="primary" loading={busy} disabled={!company || !definition || !from || !may("create")} onClick={() => void start()}>
          {t("Work out the return")}
        </Button>
      </div>
    </div>
  );
}

function Desk({ id }: { id: string }) {
  const qc = useQueryClient();
  const may = useRights("TaxReturn").may;
  const [sp, setSp] = useSearchParams();
  const open = sp.get("box");
  const [adj, setAdj] = useState<Adjust>({});
  const [busy, setBusy] = useState<null | "save" | "file" | "reopen">(null);
  const [problem, setProblem] = useState<string | null>(null);
  const q = useQuery({ queryKey: ["erp", "return", id], queryFn: ({ signal }) => api.get<Return>(docPath("TaxReturn", id), undefined, signal) });
  const r = q.data;
  const def = useQuery({ queryKey: ["erp", "return-def", r?.definition], enabled: !!r, queryFn: ({ signal }) => api.get<Definition>(docPath("TaxReturnDefinition", r?.definition ?? ""), undefined, signal) });
  const choices = useQuery({
    queryKey: ["erp", "return-files", id, r?.docstatus],
    enabled: !!r,
    queryFn: ({ signal }) => api.get<{ templates: { key: string; label?: string | null; output: string }[] }>(`/api/documents/choices/TaxReturn`, { record: id }, signal),
  });

  const keep = (x: Return) => {
    qc.setQueryData(["erp", "return", id], x);
    qc.removeQueries({ queryKey: ["erp", "return-drill", id] });
    setAdj({});
  };
  const run = async (what: NonNullable<typeof busy>, go: () => Promise<void>) => {
    if (busy) return;
    setBusy(what);
    setProblem(null);
    try {
      await go();
    } catch (e) {
      setProblem(errorMessage(e));
    } finally {
      setBusy(null);
    }
  };

  if (q.error) return <LoadFailed error={q.error} onRetry={() => void q.refetch()} title={t("Could not open the return")} />;
  if (!r) return <Loading label={t("Opening the return")} />;
  const s = status(r);
  const draft = r.docstatus === 0;
  const missing = needReason(r.boxes, adj);
  const mayEdit = draft && may("write");

  const save = () =>
    run("save", async () => {
      keep(await api.patch<Return>(docPath("TaxReturn", id), adjustBody(r.boxes, adj), { ifMatch: r.version }));
      toast(t("Saved"));
    });
  const file = async () => {
    const ok = await confirmDialog({
      title: t("File this return?"),
      message: t("Its values are kept as they stand, and no invoice dated from {from} to {to} can be submitted or cancelled afterwards. To change them, cancel the return first.", { from: formatDate(r.from_date), to: formatDate(r.to_date) }),
      confirmLabel: t("File the return"),
    });
    if (!ok) return;
    await run("file", async () => {
      let version = r.version;
      if (edited(adj)) {
        const saved = await api.patch<Return>(docPath("TaxReturn", id), adjustBody(r.boxes, adj), { ifMatch: version });
        version = saved.version;
        keep(saved);
      }
      keep(await api.post<Return>(docPath("TaxReturn", id, "submit"), {}, { ifMatch: version }));
      toast(t("Filed"));
    });
  };
  const reopen = async () => {
    const ok = await confirmDialog({ title: t("Cancel this return?"), message: t("The period is open to invoices again. The return can be made again."), confirmLabel: t("Cancel the return"), danger: true });
    if (!ok) return;
    await run("reopen", async () => keep(await api.post<Return>(docPath("TaxReturn", id, "cancel"), {}, { ifMatch: r.version })));
  };
  const files = (choices.data?.templates ?? []).filter((x) => x.output !== "page");

  return (
    <div className="erp-page">
      <PageHead
        title={r.number ? t("Tax return {number}", { number: r.number }) : t("Tax return")}
        meta={<Meta items={[<Badge key="s" tone={STATUS_TONE[s]}>{STATUS_WORD[s]()}</Badge>, `${formatDate(r.from_date)} – ${formatDate(r.to_date)}`, r.filed_on && t("Filed on {date}", { date: formatDate(r.filed_on) })]} />}
        actions={<Link className="btn btn-ghost btn-md" to={`/e/TaxReturn/${id}`}>{t("Open the record")}</Link>}
      />
      {r.docstatus === 1 && <Alert tone="info">{t("This period is closed to invoices: none dated within it can be submitted or cancelled until the return is cancelled.")}</Alert>}
      {problem && <Alert tone="error">{isolate(problem)}</Alert>}
      <div className="table-wrap">
        <table className="table" aria-label={t("Boxes")}>
          <thead>
            <tr>
              <th scope="col">{t("Box")}</th>
              <th scope="col">{t("Label")}</th>
              <th scope="col">{t("How it is worked out")}</th>
              <th scope="col" className="num">{t("From the books")}</th>
              <th scope="col" className="num">{t("Adjustment")}</th>
              <th scope="col">{t("Reason")}</th>
              <th scope="col" className="num">{t("Filed value")}</th>
            </tr>
          </thead>
          <tbody>
            {r.boxes.map((raw) => {
              const b = boxOf(raw, adj);
              const kind = kindOf(def.data?.boxes, b.code);
              const bad = missing.includes(b.code);
              return (
                <tr key={b.code} aria-selected={open === b.code || undefined}>
                  <th scope="row">
                    {drillable(def.data?.boxes, b.code) ? (
                      <button type="button" className="link-btn" aria-expanded={open === b.code} onClick={() => setSp((p) => { const n = new URLSearchParams(p); open === b.code ? n.delete("box") : n.set("box", b.code); return n; }, { replace: true })}>
                        {isolate(b.code)}
                      </button>
                    ) : (
                      isolate(b.code)
                    )}
                  </th>
                  <td>{isolate(b.label ?? "")}</td>
                  <td>{KIND_WORD(kind)}</td>
                  <td className="num">{isolate(show(b.computed))}</td>
                  <td className="num">
                    {mayEdit ? amountInput(b.adjustment, (v) => setAdj((a) => ({ ...a, [b.code]: { ...a[b.code], adjustment: v } })), { label: t("Adjustment of box {code}", { code: b.code }) }) : isolate(show(b.adjustment))}
                  </td>
                  <td>
                    {mayEdit ? (
                      <input className="input" aria-label={t("Reason for the adjustment of box {code}", { code: b.code })} aria-invalid={bad || undefined} value={b.adjustment_reason ?? ""} onChange={(e) => setAdj((a) => ({ ...a, [b.code]: { ...a[b.code], adjustment_reason: e.target.value } }))} />
                    ) : (
                      isolate(b.adjustment_reason ?? "")
                    )}
                    {bad && <p className="field-error" role="alert">{t("Say why the box is adjusted.")}</p>}
                  </td>
                  <td className="num">{isolate(show(b.value))}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      {open && <Documents ret={r} code={open} />}
      {files.length > 0 && (
        <section className="erp-panel" aria-labelledby="tr-files">
          <h2 id="tr-files" className="erp-card-title">{t("Files")}</h2>
          {draft && <p className="muted">{t("A file made now carries the values as they stand; file the return to keep them.")}</p>}
          <div className="erp-actions">
            {files.map((f) => (
              <Button key={f.key} icon="download" onClick={() => download(`/api/documents/${encodeURIComponent(f.key)}/render/${encodeURIComponent(id)}`, `${r.number ?? "return"}`).catch((e) => toast(errorMessage(e), "error"))}>
                {isolate(f.label ?? f.key)}
              </Button>
            ))}
          </div>
        </section>
      )}
      <div className="erp-sticky-foot">
        <span className="muted" aria-live="polite">{draft && missing.length > 0 ? t("{count} adjusted boxes need a reason.", { count: missing.length }) : ""}</span>
        {draft ? (
          <div className="erp-actions">
            <Button disabled={!mayEdit || !edited(adj) || missing.length > 0 || busy !== null} loading={busy === "save"} onClick={() => void save()}>{t("Save adjustments")}</Button>
            <Button variant="primary" disabled={!may("submit") || missing.length > 0 || busy !== null} loading={busy === "file"} onClick={() => void file()}>{t("File and lock the period")}</Button>
          </div>
        ) : r.docstatus === 1 ? (
          <Button disabled={!may("cancel") || busy !== null} loading={busy === "reopen"} onClick={() => void reopen()}>{t("Cancel the return")}</Button>
        ) : null}
      </div>
    </div>
  );
}

/** The documents behind one box, from the tax ledger as the server reads it. */
function Documents({ ret, code }: { ret: Return; code: string }) {
  const q = useQuery({
    queryKey: ["erp", "return-drill", ret.id, code, ret.version],
    queryFn: ({ signal }) => api.get<Drill>("/api/reports/tax_return_documents", { company: ret.company, tax_return: ret.id, box: code }, signal),
  });
  return (
    <section className="erp-panel" aria-labelledby="tr-docs">
      <h2 id="tr-docs" className="erp-card-title">{t("Documents in box {code}", { code })}</h2>
      {q.error ? (
        <LoadFailed error={q.error} onRetry={() => void q.refetch()} title={t("Could not load the documents")} />
      ) : !q.data ? (
        <Loading columns={6} rows={3} label={t("Loading the documents")} />
      ) : q.data.rows.length === 0 ? (
        <StatePanel kind="empty" title={t("No document is counted in this box")} />
      ) : (
        <div className="table-wrap">
          <table className="table">
            <thead>
              <tr>
                <th scope="col">{t("Document")}</th>
                <th scope="col">{t("Date")}</th>
                <th scope="col">{t("Party")}</th>
                <th scope="col">{t("Tax")}</th>
                <th scope="col" className="num">{t("Rate")}</th>
                <th scope="col" className="num">{t("Taxable base")}</th>
                <th scope="col" className="num">{t("Tax amount")}</th>
                <th scope="col" className="num">{t("In the box")}</th>
              </tr>
            </thead>
            <tbody>
              {q.data.rows.map((d, i) => (
                <tr key={`${d.voucher_id}-${i}`}>
                  <td><Link to={`/e/${d.voucher_type}/${d.voucher_id}`}>{isolate(d.voucher_no ?? d.voucher_id)}</Link></td>
                  <td>{formatDate(d.posting_date)}</td>
                  <td>{isolate(d.party ?? "")}</td>
                  <td>{isolate(d.tax ?? "")}</td>
                  <td className="num">{isolate(show(d.rate))}</td>
                  <td className="num">{isolate(show(d.taxable))}</td>
                  <td className="num">{isolate(show(d.tax_amount))}</td>
                  <td className="num">{isolate(show(d.counted))}</td>
                </tr>
              ))}
            </tbody>
            <tfoot>
              <tr>
                <td colSpan={7}>{t("Total in the box")}</td>
                <td className="num">{isolate(show(q.data.totals?.counted))}</td>
              </tr>
            </tfoot>
          </table>
        </div>
      )}
    </section>
  );
}
