// The withholding desk (src/withholding.rs): the categories with their dated rates and thresholds as
// the server has them; a statement per party for a period (what each document withheld or collected,
// the certificate it used), as a file to give the party; and what is payable, by category, with the
// account it is booked to. The categories and their rates are records: nothing of a country is here.

import { useQueries, useQuery } from "@tanstack/react-query";
import { useMemo, useState } from "react";
import { Link, useSearchParams } from "react-router";
import { api, docPath, formatDate, isolate, saveBlob, t, todayIso } from "@ferrum/client";
import { Badge, Button, StatePanel, TabPanel, Tabs, useShellTitle } from "@ferrum/kit";
import { LinkInput, LinkTitle, useMe } from "@ferrum/records";
import { Ctl, Meta, PageHead } from "./chrome";
import { Gate } from "./desk";
import { Loading, LoadFailed } from "./states";
import { show } from "./money";
import { byCategory, byParty, type Category, forParty, inForce, type Report, statementCsv } from "./withholdings";

const APPLIES = (a: string) => (a === "On Bill" ? t("Deducted from the bill") : a === "On Payment" ? t("Deducted from the payment") : a === "On Sale" ? t("Collected on the sale") : isolate(a));
const BASE = (b: string) => (b === "Net Total" ? t("The net total") : b === "Grand Total" ? t("The grand total") : isolate(b));

/** `/desk/withholding`. */
export function Withholding() {
  useShellTitle(t("Withholding"));
  return (
    <Gate entity="WithholdingCategory">
      <Desk />
    </Gate>
  );
}

function Desk() {
  const me = useMe().data;
  const [sp, setSp] = useSearchParams();
  const tab = sp.get("tab") ?? "categories";
  const company = sp.get("company") ?? me?.default_company ?? null;
  const from = sp.get("from") ?? "";
  const to = sp.get("to") ?? "";
  const put = (k: string, v: string | null) => setSp((p) => { const n = new URLSearchParams(p); v ? n.set(k, v) : n.delete(k); return n; }, { replace: true });
  return (
    <div className="erp-page">
      <PageHead title={t("Withholding")} meta={<Meta items={[t("Categories and their rates, what each party had withheld or collected, and what is payable.")]} />} />
      <Tabs
        id="wh"
        value={tab}
        onChange={(x) => put("tab", x === "categories" ? null : x)}
        tabs={[
          { id: "categories", label: t("Categories") },
          { id: "statement", label: t("Statement by party") },
          { id: "payable", label: t("Payable") },
        ]}
      />
      {tab !== "categories" && (
        <form className="erp-bar" onSubmit={(e) => e.preventDefault()} aria-label={t("Which period")}>
          <Ctl label={t("Company")} htmlFor="wh-company">
            <LinkInput to="Company" id="wh-company" value={company} onChange={(id) => put("company", id)} />
          </Ctl>
          <Ctl label={t("From")} htmlFor="wh-from">
            <input id="wh-from" className="input" type="date" value={from} onChange={(e) => put("from", e.target.value || null)} />
          </Ctl>
          <Ctl label={t("To")} htmlFor="wh-to">
            <input id="wh-to" className="input" type="date" value={to} onChange={(e) => put("to", e.target.value || null)} />
          </Ctl>
        </form>
      )}
      <TabPanel id="wh" tab={tab}>
        {tab === "statement" ? <Statement company={company} from={from} to={to} /> : tab === "payable" ? <Payable company={company} from={from} to={to} /> : <Categories />}
      </TabPanel>
    </div>
  );
}

function useCategories() {
  const ids = useQuery({ queryKey: ["erp", "wh-ids"], queryFn: ({ signal }) => api.get<{ rows: { id: string }[] }>(docPath("WithholdingCategory"), { fields: "id", limit: 100, sort: "category_name asc" }, signal) });
  const docs = useQueries({
    queries: (ids.data?.rows ?? []).map((r) => ({ queryKey: ["erp", "wh-category", r.id], queryFn: ({ signal }: { signal: AbortSignal }) => api.get<Category>(docPath("WithholdingCategory", r.id), undefined, signal) })),
  });
  return { ids, docs, list: docs.flatMap((d) => (d.data ? [d.data] : [])), loading: ids.isLoading || docs.some((d) => d.isLoading) };
}

function Categories() {
  const { ids, docs, list, loading } = useCategories();
  const today = todayIso();
  if (ids.error) return <LoadFailed error={ids.error} onRetry={() => void ids.refetch()} title={t("Could not load the categories")} />;
  const failed = docs.find((d) => d.error);
  if (failed?.error) return <LoadFailed error={failed.error} onRetry={() => void failed.refetch()} title={t("Could not load a category")} />;
  if (loading) return <Loading columns={4} label={t("Loading the categories")} />;
  if (list.length === 0) {
    return (
      <StatePanel kind="empty" title={t("No withholding category yet")} action={<Link className="btn btn-primary btn-md" to="/e/WithholdingCategory/new">{t("Add a category")}</Link>}>
        {t("A country's categories come with its add-on, or an accountant adds them.")}
      </StatePanel>
    );
  }
  return (
    <div className="erp-stack">
      {list.map((c) => {
        const now = inForce(c.rates, today);
        return (
          <section key={c.id} className="erp-panel" aria-labelledby={`wh-c-${c.id}`}>
            <div className="erp-panel-head">
              <div className="erp-panel-title">
                <h2 id={`wh-c-${c.id}`}>{isolate(c.category_name)}</h2>
                <p className="muted">
                  {APPLIES(c.applies)} · {t("on {base}", { base: BASE(c.base) })}
                  {c.cumulative_base === "Above Threshold" && <> · {t("only the part above the yearly threshold")}</>}
                </p>
              </div>
              <Link to={`/e/WithholdingCategory/${c.id}`}>{t("Open the record")}</Link>
            </div>
            {c.description && <p className="muted">{isolate(c.description)}</p>}
            <div className="table-wrap">
              <table className="table" aria-label={t("Rates of {name}", { name: c.category_name })}>
                <thead>
                  <tr>
                    <th scope="col">{t("From")}</th>
                    <th scope="col" className="num">{t("Rate %")}</th>
                    <th scope="col" className="num">{t("Per document threshold")}</th>
                    <th scope="col" className="num">{t("Yearly threshold")}</th>
                  </tr>
                </thead>
                <tbody>
                  {[...c.rates].sort((a, b) => b.valid_from.localeCompare(a.valid_from)).map((r, i) => (
                    <tr key={r.id ?? i}>
                      <td>
                        {formatDate(r.valid_from)} {now === r && <Badge tone="green">{t("In force")}</Badge>}
                      </td>
                      <td className="num">{isolate(show(r.rate))}</td>
                      <td className="num">{isolate(show(r.single_threshold))}</td>
                      <td className="num">{isolate(show(r.yearly_threshold))}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </section>
        );
      })}
    </div>
  );
}

function useWithheld(company: string | null, from: string, to: string) {
  return useQuery({
    queryKey: ["erp", "withheld", company, from, to],
    enabled: !!company,
    queryFn: ({ signal }) => api.get<Report>("/api/reports/withholding", { company: company ?? "", ...(from ? { from_date: from } : {}), ...(to ? { to_date: to } : {}) }, signal),
  });
}

function Statement({ company, from, to }: { company: string | null; from: string; to: string }) {
  const [party, setParty] = useState("");
  const q = useWithheld(company, from, to);
  const rows = useMemo(() => forParty(q.data?.rows ?? [], party), [q.data, party]);
  const parties = useMemo(() => byParty(rows), [rows]);
  if (!company) return <StatePanel kind="empty" title={t("Choose a company")} />;
  if (q.error) return <LoadFailed error={q.error} onRetry={() => void q.refetch()} title={t("Could not load the statement")} />;
  if (!q.data) return <Loading columns={6} label={t("Loading the statement")} />;
  const heads = [t("Category"), t("Party"), t("Date"), t("Document"), t("Against invoice"), t("Certificate"), t("Base"), t("Withheld on"), t("Rate %"), t("Amount")];
  const download = () => saveBlob(new Blob([statementCsv(rows, heads)], { type: "text/csv;charset=utf-8" }), `${party.trim() || "statement"}.csv`);
  return (
    <div className="erp-stack">
      <div className="erp-bar">
        <Ctl label={t("Party")} htmlFor="wh-party">
          <input id="wh-party" className="input" value={party} placeholder={t("All parties")} onChange={(e) => setParty(e.target.value)} />
        </Ctl>
        <Button icon="download" disabled={rows.length === 0} onClick={download}>
          {t("Download the statement")}
        </Button>
      </div>
      {q.data.meta?.truncated && <p className="muted">{t("Only the first rows are shown. Choose a shorter period.")}</p>}
      {parties.length === 0 ? (
        <StatePanel kind="empty" title={t("Nothing was withheld or collected in this period")} />
      ) : (
        parties.map((p) => (
          <section key={p.key} className="erp-panel" aria-label={p.key || t("No party")}>
            <div className="erp-panel-head">
              <h2 className="erp-card-title">{p.key ? isolate(p.key) : t("No party")}</h2>
              <span className="erp-panel-amount">{isolate(show(p.amount))}</span>
            </div>
            <div className="table-wrap">
              <table className="table">
                <thead>
                  <tr>
                    <th scope="col">{t("Category")}</th>
                    <th scope="col">{t("Date")}</th>
                    <th scope="col">{t("Document")}</th>
                    <th scope="col">{t("Certificate")}</th>
                    <th scope="col" className="num">{t("Base")}</th>
                    <th scope="col" className="num">{t("Withheld on")}</th>
                    <th scope="col" className="num">{t("Rate %")}</th>
                    <th scope="col" className="num">{t("Amount")}</th>
                  </tr>
                </thead>
                <tbody>
                  {p.lines.map((l, i) => (
                    <tr key={i}>
                      <td>{isolate(l.category)}</td>
                      <td>{formatDate(l.posting_date)}</td>
                      <td><Link to={`/e/${l.voucher_type}/${l.voucher_id}`}>{isolate(l.voucher_no ?? "")}</Link></td>
                      <td>{isolate(l.certificate ?? "")}</td>
                      <td className="num">{isolate(show(l.base))}</td>
                      <td className="num">{isolate(show(l.taxable))}</td>
                      <td className="num">{isolate(show(l.rate))}</td>
                      <td className="num">{isolate(show(l.amount))}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </section>
        ))
      )}
    </div>
  );
}

function Payable({ company, from, to }: { company: string | null; from: string; to: string }) {
  const q = useWithheld(company, from, to);
  const cats = useCategories();
  const sums = useMemo(() => byCategory(q.data?.rows ?? []), [q.data]);
  if (!company) return <StatePanel kind="empty" title={t("Choose a company")} />;
  if (q.error) return <LoadFailed error={q.error} onRetry={() => void q.refetch()} title={t("Could not load what is payable")} />;
  if (!q.data || cats.loading) return <Loading columns={4} label={t("Working out what is payable")} />;
  if (sums.length === 0) return <StatePanel kind="empty" title={t("Nothing is payable for this period")} />;
  return (
    <div className="table-wrap">
      <table className="table" aria-label={t("Payable by category")}>
        <thead>
          <tr>
            <th scope="col">{t("Category")}</th>
            <th scope="col">{t("Booked to")}</th>
            <th scope="col" className="num">{t("Base")}</th>
            <th scope="col" className="num">{t("Withheld on")}</th>
            <th scope="col" className="num">{t("To pay over")}</th>
          </tr>
        </thead>
        <tbody>
          {sums.map((s) => {
            const account = cats.list.find((c) => c.category_name === s.key)?.accounts.find((a) => a.company === company)?.account;
            return (
              <tr key={s.key}>
                <td>{isolate(s.key)}</td>
                <td>{account ? <LinkTitle to="Account" id={account} /> : <span className="muted">{t("The account with the category's number")}</span>}</td>
                <td className="num">{isolate(show(s.base))}</td>
                <td className="num">{isolate(show(s.taxable))}</td>
                <td className="num">{isolate(show(s.amount))}</td>
              </tr>
            );
          })}
        </tbody>
        <tfoot>
          <tr>
            <td colSpan={4}>{t("Total")}</td>
            <td className="num">{isolate(show(q.data.totals?.amount))}</td>
          </tr>
        </tfoot>
      </table>
    </div>
  );
}
