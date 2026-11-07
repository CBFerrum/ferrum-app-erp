// The financial statements: the profit and loss, the balance sheet and the cash flow, for a
// period picked by name, set beside the periods before it, narrowed to a cost centre or project.
// Every figure opens the ledger entries behind it. The server adds the sums up (and says when a
// statement does not balance, or the cash flow does not end where the bank does); this screen
// lays them out and keeps the choices in the address, so a link is the statement.

import { useQuery } from "@tanstack/react-query";
import { type ReactNode, useCallback, useMemo, useState } from "react";
import { Link, useSearchParams } from "react-router";
import { api, formatDate, saveBlob, todayIso, t as tr, isolate } from "@ferrum/client";
import { Alert, Button, Dialog, EmptyState, ErrorState, Field, PageLoading, useShellTitle } from "@ferrum/kit";
import { LinkInput, useMe, useMetaIndex } from "@ferrum/records";
import { sub } from "./decimal";
import { show } from "./money";
import { amountColumns, days, type Drill, KINDS, type Kind, ledgerHref, ledgerQuery, moved, named, periodOf, type PeriodInfo, type Preset, presets, type Row, type Statement as Data, toCsv } from "./statements";
import { AccountsTabs } from "./Tabs";

/** What is added up at the top, per statement: [label, key in a period's figures]. */
const HEADLINES: Record<Kind, [string, string][]> = {
  profit_and_loss: [["Income", "income"], ["Expenses", "expense"], ["Net profit", "net_profit"]],
  balance_sheet: [["Assets", "assets"], ["Liabilities", "liabilities"], ["Equity", "equity"]],
  cash_flow: [["Operating", "operating"], ["Investing", "investing"], ["Financing", "financing"], ["Net change in cash", "net_change"]],
};

interface Ledger {
  rows: (Row & { posting_date?: string; voucher_type?: string; voucher_no?: string; voucher_id?: string; party?: string; debit?: string; credit?: string; balance?: string; remarks?: string })[];
  meta: { truncated?: boolean };
}

// ───────────────────────────── the entries behind a figure ─────────────────────────────

function Entries({ title, drill, onClose }: { title: string; drill: Drill; onClose: () => void }) {
  const entities = useMetaIndex().data?.entities;
  const q = useQuery({
    queryKey: ["erp", "ledger", drill],
    queryFn: ({ signal }) => api.get<Ledger>("/api/reports/general_ledger", ledgerQuery(drill), signal),
  });
  const known = new Set(entities?.map((e) => e.name));
  return (
    <Dialog open wide onClose={onClose} title={title} footer={<Link className="btn btn-default btn-md" to={ledgerHref(drill)}>{tr("Open in the general ledger")}</Link>}>
      {q.error ? (
        <ErrorState error={q.error} onRetry={() => void q.refetch()} title={tr("Could not load the entries")} />
      ) : !q.data ? (
        <PageLoading label={tr("Loading the entries")} />
      ) : q.data.rows.length <= 2 ? (
        <EmptyState title={tr("No entries in this period")} icon="list" />
      ) : (
        <div className="table-wrap erp-entries-table">
          <table className="table">
            <thead>
              <tr>
                <th>{tr("Date")}</th>
                <th>{tr("Entry")}</th>
                <th>{tr("Account")}</th>
                <th className="num">{tr("Debit")}</th>
                <th className="num">{tr("Credit")}</th>
                <th className="num">{tr("Balance")}</th>
              </tr>
            </thead>
            <tbody>
              {q.data.rows.map((r, i) => (
                <tr key={i} className={r._bold ? "bold" : undefined}>
                  <td className="nowrap">{r.posting_date ? formatDate(r.posting_date) : ""}</td>
                  <td>
                    {r.voucher_type && r.voucher_id && known.has(r.voucher_type) ? (
                      <Link to={`/e/${r.voucher_type}/${r.voucher_id}`}>{r.voucher_no || r.voucher_type}</Link>
                    ) : (
                      (r.voucher_no ?? "")
                    )}
                    {r.party && <div className="faint">{isolate(r.party)}</div>}
                  </td>
                  <td>{isolate(r.account)}</td>
                  <td className="num">{r.debit && r.debit !== "0" ? show(r.debit) : ""}</td>
                  <td className="num">{r.credit && r.credit !== "0" ? show(r.credit) : ""}</td>
                  <td className="num">{isolate(show(r.balance))}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {q.data?.meta.truncated && <p className="faint">{tr("Only the first entries are shown. Open the general ledger for a narrower look.")}</p>}
    </Dialog>
  );
}

// ───────────────────────────── the screen ─────────────────────────────

export function Statement({ kind }: { kind: Kind }) {
  const info = KINDS.find((k) => k.id === kind)!;
  useShellTitle(info.label);
  const me = useMe().data;
  const [sp, setSp] = useSearchParams();
  const startMonth = me?.fiscal_year_start_month ?? 4;
  const today = todayIso();
  const stretch = useMemo(() => presets(today, startMonth), [today, startMonth]);
  const points = useMemo(() => days(today, startMonth), [today, startMonth]);
  const thisYear = stretch.find((p) => p.id === "this-year")!;
  const company = sp.get("company") ?? me?.default_company ?? null;
  const asOf = sp.get("as_of") ?? today;
  const from = sp.get("from_date") ?? thisYear.from;
  const to = sp.get("to_date") ?? thisYear.to;
  const compare = sp.get("compare") ?? "";
  const many = sp.get("periods") ?? "1";
  const costCenter = sp.get("cost_center");
  const project = sp.get("project");
  const [drill, setDrill] = useState<{ title: string; drill: Drill } | null>(null);

  const put = useCallback(
    (changes: Record<string, string | null>) =>
      setSp(
        (prev) => {
          const next = new URLSearchParams(prev);
          for (const [k, v] of Object.entries(changes)) v ? next.set(k, v) : next.delete(k);
          return next;
        },
        { replace: true },
      ),
    [setSp],
  );
  const params = useMemo(() => {
    const q: Record<string, string> = kind === "balance_sheet" ? { as_of: asOf } : { from_date: from, to_date: to };
    if (company) q.company = company;
    if (compare) {
      q.compare = compare;
      q.periods = many;
    }
    if (kind === "profit_and_loss") {
      if (costCenter) q.cost_center = costCenter;
      if (project) q.project = project;
    }
    return q;
  }, [kind, asOf, from, to, company, compare, many, costCenter, project]);
  const q = useQuery({
    queryKey: ["erp", "statement", kind, params],
    enabled: !!me,
    queryFn: ({ signal }) => api.get<Data>(`/api/reports/${kind}`, params, signal),
  });
  const data = q.data;

  const open = (row: Row, column: string) => {
    const period = data?.meta.periods.find((p) => p.key === column);
    if (!row.account_id || !period || !company) return;
    const { from: f, to: t } = periodOf(kind, period, startMonth);
    setDrill({ title: `${row.account}, ${period.label}`, drill: { company, account: row.account_id, from: f, to: t, excludeClosing: kind === "profit_and_loss", costCenter, project } });
  };

  const chooser: Preset[] = kind === "balance_sheet" ? points : stretch;
  const current = kind === "balance_sheet" ? named(points, asOf, asOf) : named(stretch, from, to);
  const columns = data ? amountColumns(data.columns) : [];
  const compared = columns.length > 1;

  let body: ReactNode;
  if (q.error) body = <ErrorState error={q.error} onRetry={() => void q.refetch()} title={tr("Could not run the statement")} />;
  else if (!data) body = <PageLoading label={tr("Running the statement")} />;
  else if (kind === "profit_and_loss" && data.rows.length <= 5) {
    body = (
      <EmptyState title={tr("Nothing was posted in this period")} icon="chart">
        <p className="muted">{tr("Pick another period, or check the company and the filters above.")}</p>
      </EmptyState>
    );
  } else {
    body = (
      <>
        {data.meta.balanced === false && <Alert tone="error">{tr("The balance sheet is out of balance by")} {isolate(show(data.meta.difference))}{tr(". This should never happen: tell whoever looks after the books.")}</Alert>}
        {data.meta.reconciled === false && <Alert tone="error">{tr("The cash flow ends")} {isolate(show(data.meta.difference))} {tr("away from the bank and cash accounts. This should never happen: tell whoever looks after the books.")}</Alert>}
        <Headlines kind={kind} data={data} />
        <div className="table-wrap erp-statement">
          <table className="table">
            <thead>
              <tr>
                {data.columns
                  .filter((c) => c.name !== "change_pct")
                  .map((c, i) => (
                    <th key={c.name} scope="col" className={i === 0 ? "erp-first" : "num"}>
                      {isolate(c.label)}
                    </th>
                  ))}
              </tr>
            </thead>
            <tbody>
              {data.rows.map((r, i) => (
                <tr key={i} className={r._bold ? "bold" : undefined}>
                  {data.columns.filter((c) => c.name !== "change_pct").map((c, ci) => {
                    if (ci === 0) {
                      return (
                        <th key={c.name} scope="row" className="erp-first erp-account" style={{ paddingLeft: `${0.75 + (r._indent ?? 0) * 1.1}rem` }}>
                          {isolate(String(r[c.name] ?? ""))}
                        </th>
                      );
                    }
                    const v = r[c.name];
                    if (c.name === "change") return <td key={c.name} className="num">{isolate(moved(r.change as string, r.change_pct as string | null, show))}</td>;
                    if (v === undefined || v === null || v === "") return <td key={c.name} />;
                    return (
                      <td key={c.name} className="num">
                        {r.account_id ? (
                          <button type="button" className="erp-fig" title={tr("Show the entries behind this figure")} onClick={() => open(r, c.name)}>
                            {isolate(show(v))}
                          </button>
                        ) : (
                          show(v)
                        )}
                      </td>
                    );
                  })}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
        <p className="faint">
          {kind === "cash_flow"
            ? tr("Changes in the other accounts are sorted by the account's type, or by the cash flow section set on the account.")
            : compared
              ? tr("Click a figure to see the entries behind it.")
              : tr("Click a figure to see the entries behind it, or set the statement beside an earlier period.")}
        </p>
      </>
    );
  }

  const save = () => data && saveBlob(new Blob([`﻿${toCsv(data)}`], { type: "text/csv;charset=utf-8" }), `${kind}-${kind === "balance_sheet" ? asOf : to}.csv`);
  return (
    <div className="erp-page erp-statements">
      <header className="erp-head">
        <div>
          <AccountsTabs />
          <p className="page-sub">{kind === "balance_sheet" ? tr("As of {asOf}", { asOf: formatDate(asOf) }) : tr("{from} to {to}", { from: formatDate(from), to: formatDate(to) })}</p>
        </div>
        <div className="erp-tools">
          <div className="segmented" role="group" aria-label={tr("Statement")}>
            {KINDS.map((k) => (
              <Link key={k.id} to={{ pathname: k.path, search: sp.toString() }} aria-current={k.id === kind ? "page" : undefined} className="erp-seg" data-on={k.id === kind || undefined}>
                {isolate(k.label)}
              </Link>
            ))}
          </div>
          <Button icon="download" onClick={save} disabled={!data}>
            {tr("Download CSV")}
          </Button>
        </div>
      </header>

      <form className="erp-filters" onSubmit={(e) => e.preventDefault()} aria-label={tr("What the statement covers")}>
        <Field label={kind === "balance_sheet" ? tr("As of") : tr("Period")} htmlFor="st-period">
          <select
            id="st-period"
            className="select"
            value={current}
            onChange={(e) => {
              const p = chooser.find((x) => x.id === e.target.value);
              if (p) put(kind === "balance_sheet" ? { as_of: p.to } : { from_date: p.from, to_date: p.to });
            }}
          >
            {chooser.map((p) => (
              <option key={p.id} value={p.id}>
                {isolate(p.label)}
              </option>
            ))}
            <option value="custom">{tr("Choose dates…")}</option>
          </select>
        </Field>
        {kind === "balance_sheet" ? (
          <Field label={tr("Date")} htmlFor="st-as-of">
            <input id="st-as-of" className="input" type="date" value={asOf} onChange={(e) => put({ as_of: e.target.value || null })} />
          </Field>
        ) : (
          <>
            <Field label={tr("From")} htmlFor="st-from">
              <input id="st-from" className="input" type="date" value={from} max={to} onChange={(e) => put({ from_date: e.target.value || null })} />
            </Field>
            <Field label={tr("To")} htmlFor="st-to">
              <input id="st-to" className="input" type="date" value={to} min={from} onChange={(e) => put({ to_date: e.target.value || null })} />
            </Field>
          </>
        )}
        <Field label={tr("Compare with")} htmlFor="st-compare">
          <select id="st-compare" className="select" value={compare} onChange={(e) => put({ compare: e.target.value || null, periods: null })}>
            <option value="">{tr("Nothing")}</option>
            <option value="Previous period">{tr("The period before")}</option>
            <option value="Previous year">{tr("The same period a year before")}</option>
          </select>
        </Field>
        {compare && (
          <Field label={tr("How many")} htmlFor="st-many">
            <select id="st-many" className="select" value={many} onChange={(e) => put({ periods: e.target.value === "1" ? null : e.target.value })}>
              {["1", "2", "3"].map((n) => (
                <option key={n}>{isolate(n)}</option>
              ))}
            </select>
          </Field>
        )}
        <Field label={tr("Company")} htmlFor="st-company">
          <LinkInput to="Company" id="st-company" value={company} onChange={(id) => put({ company: id })} />
        </Field>
        {kind === "profit_and_loss" && (
          <>
            <Field label={tr("Cost centre")} htmlFor="st-cc" hint={tr("With the ones below it.")}>
              <LinkInput to="CostCenter" id="st-cc" value={costCenter} placeholder={tr("All")} onChange={(id) => put({ cost_center: id })} />
            </Field>
            <Field label={tr("Project")} htmlFor="st-project">
              <LinkInput to="Project" id="st-project" value={project} placeholder={tr("All")} onChange={(id) => put({ project: id })} />
            </Field>
          </>
        )}
      </form>

      {body}
      {drill && <Entries title={drill.title} drill={drill.drill} onClose={() => setDrill(null)} />}
    </div>
  );
}

/** What each statement adds up to, with how the first period moved from the second. */
function Headlines({ kind, data }: { kind: Kind; data: Data }) {
  const [now, before] = data.meta.periods as [PeriodInfo, PeriodInfo | undefined];
  return (
    <dl className="erp-headlines">
      {HEADLINES[kind].map(([label, key]) => {
        const v = now[key] as string | undefined;
        if (v === undefined) return null;
        const was = before?.[key] as string | undefined;
        return (
          <div key={key} className="erp-headline">
            <dt>{isolate(label)}</dt>
            <dd>{isolate(show(v))}</dd>
            {was !== undefined && <dd className="faint">{tr("{value} from {label}", { value: moved(sub(v, was), null, show), label: before!.label })}</dd>}
          </div>
        );
      })}
    </dl>
  );
}

export const ProfitAndLoss = () => <Statement kind="profit_and_loss" />;
export const BalanceSheet = () => <Statement kind="balance_sheet" />;
export const CashFlow = () => <Statement kind="cash_flow" />;
