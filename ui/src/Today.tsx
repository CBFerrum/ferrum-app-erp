// The finance lead's morning check: what is overdue and what falls due this week, bank lines waiting
// to be reconciled, drafts waiting to be submitted, months and years that have ended and are not
// closed, and the cash there is. Every figure leads to the place that deals with it; an Accounts
// Manager closes a month from here. The counting is the server's (the Finance Overview report, which
// counts only what the person may read); this screen decides nothing about the books.

import { useQuery, useQueryClient } from "@tanstack/react-query";
import { type ReactNode, useCallback } from "react";
import { Link, useSearchParams } from "react-router";
import { api, docPath, errorMessage, formatDate, todayIso, t, isolate } from "@ferrum/client";
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
        {isolate(title)}
      </h2>
      {children}
      {to && link && (
        <Link className="erp-attn-link" to={to}>
          {isolate(link)}
        </Link>
      )}
    </section>
  );
}

export function Today() {
  useShellTitle(t("Accounts today"));
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
    const ok = await confirmDialog({ title: t("Close {start}?", { start: monthName(start) }), message: t("Postings dated in it are refused from then on, until it is opened again."), confirmLabel: t("Close the month") });
    if (!ok) return;
    try {
      await api.post(docPath("FiscalYear", fiscalYear, "actions", "close_period"), { period_start: start });
      toast(t("{start} closed", { start: monthName(start) }));
      refresh();
    } catch (e) {
      toast(errorMessage(e), "error");
    }
  };

  let body: ReactNode;
  if (q.error) body = <ErrorState error={q.error} onRetry={() => void q.refetch()} title={t("Could not load the overview")} />;
  else if (!q.data) body = <PageLoading label={t("Looking at the books")} />;
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
          {pressing === 0 ? t("Everything is in hand.") : t("{pressing} to look at.", { pressing: plural(pressing, "thing") })}
        </p>
        <div className="erp-today-grid">
          <Card title={t("Owed to you")} calm={(late?.count ?? 0) === 0} to={destination(late ?? soon!, company, asOf)} link="Open the receivables">
            {(late?.count ?? 0) > 0 ? (
              <>
                <p className="erp-figure-big erp-negative">{isolate(show(late!.amount))}</p>
                <p className="muted">{t("overdue: {late}", { late: parts(late) })}</p>
              </>
            ) : (
              <p className="muted">{t("Nothing is overdue.")}</p>
            )}
            {(soon?.count ?? 0) > 0 && (
              <p>
                <strong>{isolate(show(soon!.amount))}</strong> <span className="muted">{t("falls due this week ({value})", { value: plural(soon!.count!, "invoice") })}</span>
              </p>
            )}
          </Card>
          <Card title={t("Owed by you")} calm={(owed?.count ?? 0) === 0} to={destination(owed ?? owedSoon!, company, asOf)} link="Open the payables">
            {(owed?.count ?? 0) > 0 ? (
              <>
                <p className="erp-figure-big erp-negative">{isolate(show(owed!.amount))}</p>
                <p className="muted">{t("overdue: {owed}", { owed: parts(owed, "bill") })}</p>
              </>
            ) : (
              <p className="muted">{t("Nothing is overdue.")}</p>
            )}
            {(owedSoon?.count ?? 0) > 0 && (
              <p>
                <strong>{isolate(show(owedSoon!.amount))}</strong> <span className="muted">{t("falls due this week ({value})", { value: plural(owedSoon!.count!, "bill") })}</span>
              </p>
            )}
          </Card>
          {lines && (
            <Card title={t("Bank")} calm={(lines.count ?? 0) === 0} to={destination(lines, company, asOf)} link="Reconcile">
              {(lines.count ?? 0) > 0 ? (
                <>
                  <p className="erp-figure-big">{isolate(plural(lines.count!, "line"))}</p>
                  <p className="muted">{t("of the bank statements are not reconciled")}{lines.oldest ? t(", the oldest from {value}", { value: formatDate(lines.oldest) }) : ""}.</p>
                  <ul className="erp-attn-list">
                    {(lines.detail ?? [])
                      .filter((d) => (d.count ?? 0) > 0)
                      .map((d) => (
                        <li key={d.id}>
                          <Link to={`/r/bank_reconciliation?account=${d.id}`}>{isolate(d.name ?? "")}</Link>
                          <span className="faint">{d.count}</span>
                        </li>
                      ))}
                  </ul>
                </>
              ) : (
                <p className="muted">{t("Every statement line is reconciled.")}</p>
              )}
            </Card>
          )}
          <Card title={t("Drafts")} calm={(drafts?.count ?? 0) === 0}>
            {(drafts?.count ?? 0) > 0 ? (
              <>
                <p className="erp-figure-big">{isolate(plural(drafts!.count!, "draft"))}</p>
                <p className="muted">{t("waiting to be submitted")}{drafts!.oldest ? t(", the oldest dated {value}", { value: formatDate(drafts!.oldest) }) : ""}.</p>
                <ul className="erp-attn-list">
                  {(drafts!.detail ?? []).map((d) => (
                    <li key={d.entity}>
                      <Link to={draftsPath(d.entity!)}>{isolate(d.label ?? "")}</Link>
                      <span className="faint">{d.count}</span>
                    </li>
                  ))}
                </ul>
              </>
            ) : (
              <p className="muted">{t("Nothing is waiting to be submitted.")}</p>
            )}
          </Card>
          <Card title={t("Closing the books")} calm={(months?.count ?? 0) === 0 && (years?.count ?? 0) === 0}>
            {(months?.count ?? 0) === 0 && (years?.count ?? 0) === 0 && <p className="muted">{t("Every month that has ended is closed.")}</p>}
            {(months?.count ?? 0) > 0 && (
              <>
                <p className="erp-figure-big">{isolate(plural(months!.count!, "month"))}</p>
                <p className="muted">
                  {t("ended and not closed")}{months!.oldest ? t(", back to {value}", { value: monthName(months!.oldest) }) : ""}{t(". The latest:")}
                </p>
                <ul className="erp-attn-list">
                  {(months!.detail ?? []).slice(0, 3).map((d) => (
                    <li key={d.start}>
                      <span>{isolate(monthName(d.start!))}</span>
                      {manager && d.fiscal_year ? (
                        <Button size="sm" onClick={() => void close(d.fiscal_year!, d.start!)}>
                          {t("Close")}
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
                      {t("Fiscal year {name} ended", { name: y.name ?? "" })} {y.end ? formatDate(y.end) : ""}
                    </span>
                    {manager && (
                      <Link className="btn btn-default btn-sm" to={`/e/PeriodClosingVoucher/new?fiscal_year=${y.id}`}>
                        {t("Close the year")}
                      </Link>
                    )}
                  </li>
                ))}
              </ul>
            )}
          </Card>
          {cash && (
            <Card title={t("Cash and bank")} calm>
              <p className="erp-figure-big">{isolate(show(cash.amount))}</p>
              <ul className="erp-attn-list">
                {(cash.detail ?? []).map((d) => (
                  <li key={d.id}>
                    <span>{isolate(d.name ?? "")}</span>
                    <span className="num">{isolate(show(d.balance))}</span>
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
          <p className="page-sub">{isolate(new Date(`${asOf}T00:00:00`).toLocaleDateString(undefined, { weekday: "long", day: "numeric", month: "long", year: "numeric" }))}</p>
        </div>
        <div className="erp-tools">
          <div className="erp-pick">
            <LinkInput to="Company" id="td-company" label={t("Company")} value={company} onChange={(id) => put({ company: id })} />
          </div>
          <label className="erp-pick">
            <span className="sr-only">{t("Look as of")}</span>
            <input className="input" type="date" value={asOf} onChange={(e) => put({ as_of: e.target.value || null })} />
          </label>
        </div>
      </header>
      {!company && me ? (
        <EmptyState title={t("Choose a company")} icon="home">
          <p className="muted">{t("The overview is for one company at a time.")}</p>
        </EmptyState>
      ) : (
        body
      )}
    </div>
  );
}
