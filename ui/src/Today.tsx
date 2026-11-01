// The finance lead's morning check: what is overdue and what falls due this week, bank lines waiting
// to be reconciled, drafts waiting to be submitted, months and years that have ended and are not
// closed, and the cash there is. Every figure leads to the place that deals with it; an Accounts
// Manager closes a month from here. The counting is the server's (the Finance Overview report, which
// counts only what the person may read); this screen decides nothing about the books.

import { useQuery, useQueryClient } from "@tanstack/react-query";
import { type ReactNode, useCallback } from "react";
import { Link, useSearchParams } from "react-router";
import { api, docPath, errorMessage, formatDate, todayIso } from "@ferrum/client";
import { Button, confirmDialog, EmptyState, ErrorState, Icon, PageLoading, toast, useLiveEntity, useShellTitle } from "@ferrum/kit";
import { LinkInput, useMe } from "@ferrum/records";
import { destination, draftsPath, monthName, needing, type Overview, plural, row } from "./attention";
import { show } from "./money";
import { AccountsTabs } from "./Tabs";

const ROOT = ["erp", "overview"] as const;

function Card({ title, calm, children, to, link }: { title: string; calm: boolean; children: ReactNode; to?: string | null; link?: string }) {
  return (
    <section className="erp-attn" data-calm={calm || undefined} aria-label={title}>
      <h2>
        <Icon name={calm ? "check" : "alert"} size={15} />
        {title}
      </h2>
      {children}
      {to && link && (
        <Link className="erp-attn-link" to={to}>
          {link}
        </Link>
      )}
    </section>
  );
}

export function Today() {
  useShellTitle("Accounts today");
  const qc = useQueryClient();
  const me = useMe().data;
  const [sp, setSp] = useSearchParams();
  const company = sp.get("company") ?? me?.default_company ?? null;
  const asOf = sp.get("as_of") ?? todayIso();
  const manager = !!me?.user.is_admin || !!me?.user.roles.includes("Accounts Manager");
  const q = useQuery({
    queryKey: [...ROOT, company, asOf],
    enabled: !!me,
    queryFn: ({ signal }) => api.get<Overview>("/api/reports/finance_overview", { company, as_of: asOf }, signal),
  });
  const refresh = useCallback(() => void qc.invalidateQueries({ queryKey: ROOT }), [qc]);
  useLiveEntity("BankTransaction", refresh);
  useLiveEntity("SalesInvoice", refresh);
  useLiveEntity("PurchaseInvoice", refresh);
  useLiveEntity("PaymentEntry", refresh);
  useLiveEntity("JournalEntry", refresh);
  const put = (changes: Record<string, string | null>) =>
    setSp(
      (prev) => {
        const next = new URLSearchParams(prev);
        for (const [k, v] of Object.entries(changes)) v ? next.set(k, v) : next.delete(k);
        return next;
      },
      { replace: true },
    );

  const close = async (fiscalYear: string, start: string) => {
    const ok = await confirmDialog({ title: `Close ${monthName(start)}?`, message: "Postings dated in it are refused from then on, until it is opened again.", confirmLabel: "Close the month" });
    if (!ok) return;
    try {
      await api.post(docPath("FiscalYear", fiscalYear, "actions", "close_period"), { period_start: start });
      toast(`${monthName(start)} closed`);
      refresh();
    } catch (e) {
      toast(errorMessage(e), "error");
    }
  };

  let body: ReactNode;
  if (q.error) body = <ErrorState error={q.error} onRetry={() => void q.refetch()} title="Could not load the overview" />;
  else if (!q.data) body = <PageLoading label="Looking at the books" />;
  else {
    const o = q.data;
    const get = (key: string) => row(o, key);
    const [late, soon, owed, owedSoon] = [get("overdue_receivables"), get("receivables_due_week"), get("overdue_payables"), get("payables_due_week")];
    const lines = get("unreconciled_lines");
    const drafts = get("drafts");
    const months = get("open_months");
    const years = get("unclosed_years");
    const cash = get("cash");
    const pressing = needing(o).length;
    const parts = (a?: { count?: number; oldest?: string | null }, noun = "invoice") => ((a?.count ?? 0) > 0 ? `${plural(a!.count!, noun)}${a!.oldest ? `, the oldest due ${formatDate(a!.oldest)}` : ""}` : "");
    body = (
      <>
        <p className="page-sub" aria-live="polite">
          {pressing === 0 ? "Everything is in hand." : `${plural(pressing, "thing")} to look at.`}
        </p>
        <div className="erp-today-grid">
          <Card title="Owed to you" calm={(late?.count ?? 0) === 0} to={destination(late ?? soon!, company, asOf)} link="Open the receivables">
            {(late?.count ?? 0) > 0 ? (
              <>
                <p className="erp-figure-big erp-negative">{show(late!.amount)}</p>
                <p className="muted">overdue: {parts(late)}</p>
              </>
            ) : (
              <p className="muted">Nothing is overdue.</p>
            )}
            {(soon?.count ?? 0) > 0 && (
              <p>
                <strong>{show(soon!.amount)}</strong> <span className="muted">{`falls due this week (${plural(soon!.count!, "invoice")})`}</span>
              </p>
            )}
          </Card>
          <Card title="Owed by you" calm={(owed?.count ?? 0) === 0} to={destination(owed ?? owedSoon!, company, asOf)} link="Open the payables">
            {(owed?.count ?? 0) > 0 ? (
              <>
                <p className="erp-figure-big erp-negative">{show(owed!.amount)}</p>
                <p className="muted">overdue: {parts(owed, "bill")}</p>
              </>
            ) : (
              <p className="muted">Nothing is overdue.</p>
            )}
            {(owedSoon?.count ?? 0) > 0 && (
              <p>
                <strong>{show(owedSoon!.amount)}</strong> <span className="muted">{`falls due this week (${plural(owedSoon!.count!, "bill")})`}</span>
              </p>
            )}
          </Card>
          {lines && (
            <Card title="Bank" calm={(lines.count ?? 0) === 0} to={destination(lines, company, asOf)} link="Reconcile">
              {(lines.count ?? 0) > 0 ? (
                <>
                  <p className="erp-figure-big">{plural(lines.count!, "line")}</p>
                  <p className="muted">of the bank statements are not reconciled{lines.oldest ? `, the oldest from ${formatDate(lines.oldest)}` : ""}.</p>
                  <ul className="erp-attn-list">
                    {(lines.detail ?? [])
                      .filter((d) => (d.count ?? 0) > 0)
                      .map((d) => (
                        <li key={d.id}>
                          <Link to={`/r/bank_reconciliation?account=${d.id}`}>{d.name}</Link>
                          <span className="faint">{d.count}</span>
                        </li>
                      ))}
                  </ul>
                </>
              ) : (
                <p className="muted">Every statement line is reconciled.</p>
              )}
            </Card>
          )}
          <Card title="Drafts" calm={(drafts?.count ?? 0) === 0}>
            {(drafts?.count ?? 0) > 0 ? (
              <>
                <p className="erp-figure-big">{plural(drafts!.count!, "draft")}</p>
                <p className="muted">waiting to be submitted{drafts!.oldest ? `, the oldest dated ${formatDate(drafts!.oldest)}` : ""}.</p>
                <ul className="erp-attn-list">
                  {(drafts!.detail ?? []).map((d) => (
                    <li key={d.entity}>
                      <Link to={draftsPath(d.entity!)}>{d.label}</Link>
                      <span className="faint">{d.count}</span>
                    </li>
                  ))}
                </ul>
              </>
            ) : (
              <p className="muted">Nothing is waiting to be submitted.</p>
            )}
          </Card>
          <Card title="Closing the books" calm={(months?.count ?? 0) === 0 && (years?.count ?? 0) === 0}>
            {(months?.count ?? 0) === 0 && (years?.count ?? 0) === 0 && <p className="muted">Every month that has ended is closed.</p>}
            {(months?.count ?? 0) > 0 && (
              <>
                <p className="erp-figure-big">{plural(months!.count!, "month")}</p>
                <p className="muted">
                  ended and not closed{months!.oldest ? `, back to ${monthName(months!.oldest)}` : ""}. The latest:
                </p>
                <ul className="erp-attn-list">
                  {(months!.detail ?? []).slice(0, 3).map((d) => (
                    <li key={d.start}>
                      <span>{monthName(d.start!)}</span>
                      {manager && d.fiscal_year ? (
                        <Button size="sm" onClick={() => void close(d.fiscal_year!, d.start!)}>
                          Close
                        </Button>
                      ) : null}
                    </li>
                  ))}
                </ul>
              </>
            )}
            {(years?.count ?? 0) > 0 && (
              <ul className="erp-attn-list">
                {years!.detail!.map((y) => (
                  <li key={y.id}>
                    <span>
                      Fiscal year {y.name} ended {y.end ? formatDate(y.end) : ""}
                    </span>
                    {manager && (
                      <Link className="btn btn-default btn-sm" to={`/e/PeriodClosingVoucher/new?fiscal_year=${y.id}`}>
                        Close the year
                      </Link>
                    )}
                  </li>
                ))}
              </ul>
            )}
          </Card>
          {cash && (
            <Card title="Cash and bank" calm>
              <p className="erp-figure-big">{show(cash.amount)}</p>
              <ul className="erp-attn-list">
                {(cash.detail ?? []).map((d) => (
                  <li key={d.id}>
                    <span>{d.name}</span>
                    <span className="num">{show(d.balance)}</span>
                  </li>
                ))}
              </ul>
            </Card>
          )}
        </div>
      </>
    );
  }

  return (
    <div className="erp-page erp-today">
      <header className="erp-head">
        <div>
          <AccountsTabs />
          <p className="page-sub">{new Date(`${asOf}T00:00:00`).toLocaleDateString(undefined, { weekday: "long", day: "numeric", month: "long", year: "numeric" })}</p>
        </div>
        <div className="erp-tools">
          <div className="erp-pick">
            <LinkInput to="Company" id="td-company" label="Company" value={company} onChange={(id) => put({ company: id })} />
          </div>
          <label className="erp-pick">
            <span className="sr-only">Look as of</span>
            <input className="input" type="date" value={asOf} onChange={(e) => put({ as_of: e.target.value || null })} />
          </label>
        </div>
      </header>
      {!company && me ? (
        <EmptyState title="Choose a company" icon="home">
          <p className="muted">The overview is for one company at a time.</p>
        </EmptyState>
      ) : (
        body
      )}
    </div>
  );
}
