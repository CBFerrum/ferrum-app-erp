// The finance lead's morning check: what is overdue and what falls due this week, bank lines waiting
// to be reconciled, drafts waiting to be submitted, months and years that have ended and are not
// closed, and the cash there is. Every figure leads to the place that deals with it; an Accounts
// Manager closes a month from here. The counting is the server's (the Finance Overview report, which
// counts only what the person may read); this screen decides nothing about the books.

import { useQuery, useQueryClient } from "@tanstack/react-query";
import { type ReactNode, useCallback } from "react";
import { Link, useSearchParams } from "react-router";
import { api, docPath, errorMessage, formatDate, todayIso, t, isolate } from "@ferrum/client";
import { Button, confirmDialog, toast, useLiveEntity, useShellTitle, placed, StatePanel } from "@ferrum/kit";
import { Loading, LoadFailed } from "./states";
import { LinkInput, useMe } from "@ferrum/records";
import { destination, draftsPath, monthName, needing, type Overview, plural, row } from "./attention";
import { show } from "./money";
import { Meta, PageHead } from "./chrome";
import { AccountsTabs } from "./Tabs";

const ROOT = ["erp", "overview"] as const;

type Tone = "danger" | "warning" | "neutral" | "calm";

/** A group of queue rows under one quiet heading. */
function Group({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="erp-group-block" aria-label={title}>
      <h2 className="erp-group">{isolate(title)}</h2>
      <div className="erp-queue">{children}</div>
    </section>
  );
}

/**
 * One thing to do: what it is and how much (the figure, in the tone of what it is), what the oldest of it is,
 * and the one place that deals with it. Detail (bank accounts, kinds of draft, months) hangs beneath.
 */
function Row({ title, tone, amount, sub, extra, to, link, children }: { title: string; tone: Tone; amount?: ReactNode; sub: ReactNode; extra?: ReactNode; to?: string | null; link?: string; children?: ReactNode }) {
  return (
    <section className="erp-q" data-tone={tone} aria-label={title}>
      <h3 className="erp-q-title">{isolate(title)}</h3>
      {amount && <p className="erp-q-amount">{amount}</p>}
      <p className="erp-q-sub muted">{sub}</p>
      {to && link && (
        <Link className="btn btn-default btn-sm erp-q-act" to={to}>
          {isolate(link)}
        </Link>
      )}
      {extra && <p className="erp-q-extra">{extra}</p>}
      {children}
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
  if (q.error) body = <LoadFailed error={q.error} onRetry={() => void q.refetch()} title={t("Could not load the overview")} />;
  else if (!q.data) body = <Loading label={t("Looking at the books")} />;
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
    const parts = (a?: { count?: number; oldest?: string | null }, bills = false) => {
      const n = a?.count ?? 0;
      const oldest = a?.oldest ? formatDate(a.oldest) : "";
      if (n <= 0) return "";
      if (bills) return n === 1 ? (oldest ? t("1 bill, due {oldest}", { oldest }) : t("1 bill")) : oldest ? t("{n} bills, the oldest due {oldest}", { n, oldest }) : t("{n} bills", { n });
      return n === 1 ? (oldest ? t("1 invoice, due {oldest}", { oldest }) : t("1 invoice")) : oldest ? t("{n} invoices, the oldest due {oldest}", { n, oldest }) : t("{n} invoices", { n });
    };
    const calmLate = (late?.count ?? 0) === 0;
    const calmOwed = (owed?.count ?? 0) === 0;
    body = (
      <>
        <p className="muted" aria-live="polite">
          {pressing === 0 ? t("Everything is in hand.") : t("{pressing} to look at.", { pressing: plural(pressing, "thing") })}
        </p>
        <div className="erp-today-grid">
          <div className="erp-today-queue">
            <Group title={t("Collect and pay")}>
              <Row
                title={t("Owed to you")}
                tone={calmLate ? "calm" : "danger"}
                amount={!calmLate && <span className="erp-neg">{isolate(show(late!.amount))}</span>}
                sub={!calmLate ? t("overdue: {late}", { late: parts(late) }) : t("Nothing is overdue.")}
                extra={(soon?.count ?? 0) > 0 && (
                  <>
                    <strong>{isolate(show(soon!.amount))}</strong> <span className="muted">{t("falls due this week ({value})", { value: plural(soon!.count!, "invoice") })}</span>
                  </>
                )}
                to={destination(late ?? soon!, company, asOf)}
                link={t("Open the receivables")}
              />
              <Row
                title={t("Owed by you")}
                tone={calmOwed ? "calm" : "danger"}
                amount={!calmOwed && <span className="erp-neg">{isolate(show(owed!.amount))}</span>}
                sub={!calmOwed ? t("overdue: {owed}", { owed: parts(owed, true) }) : t("Nothing is overdue.")}
                extra={(owedSoon?.count ?? 0) > 0 && (
                  <>
                    <strong>{isolate(show(owedSoon!.amount))}</strong> <span className="muted">{t("falls due this week ({value})", { value: plural(owedSoon!.count!, "bill") })}</span>
                  </>
                )}
                to={destination(owed ?? owedSoon!, company, asOf)}
                link={t("Open the payables")}
              />
            </Group>
            <Group title={t("To clear")}>
              {lines && (
                <Row
                  title={t("Bank")}
                  tone={(lines.count ?? 0) === 0 ? "calm" : "warning"}
                  amount={(lines.count ?? 0) > 0 && isolate(plural(lines.count!, "line"))}
                  sub={(lines.count ?? 0) > 0 ? <>{(lines.oldest ? t("of the bank statements are not reconciled, the oldest from {value}", { value: formatDate(lines.oldest) }) : t("of the bank statements are not reconciled"))}.</> : t("Every statement line is reconciled.")}
                  to={destination(lines, company, asOf)}
                  link={t("Reconcile")}
                >
                  {(lines.count ?? 0) > 0 && (
                    <ul className="erp-attn-list">
                      {(lines.detail ?? [])
                        .filter((d) => (d.count ?? 0) > 0)
                        .map((d) => (
                          <li key={d.id}>
                            <Link to={`/r/bank_reconciliation?account=${d.id}`}>{isolate(d.name ?? "")}</Link>
                            <span className="faint num">{d.count}</span>
                          </li>
                        ))}
                    </ul>
                  )}
                </Row>
              )}
              <Row
                title={t("Drafts")}
                tone={(drafts?.count ?? 0) === 0 ? "calm" : "warning"}
                amount={(drafts?.count ?? 0) > 0 && isolate(plural(drafts!.count!, "draft"))}
                sub={(drafts?.count ?? 0) > 0 ? <>{(drafts!.oldest ? t("waiting to be submitted, the oldest dated {value}", { value: formatDate(drafts!.oldest) }) : t("waiting to be submitted"))}.</> : t("Nothing is waiting to be submitted.")}
              >
                {(drafts?.count ?? 0) > 0 && (
                  <ul className="erp-attn-list">
                    {(drafts!.detail ?? []).map((d) => (
                      <li key={d.entity}>
                        <Link to={draftsPath(d.entity!)}>{isolate(d.label ?? "")}</Link>
                        <span className="faint num">{d.count}</span>
                      </li>
                    ))}
                  </ul>
                )}
              </Row>
            </Group>
            <Group title={t("Closing the books")}>
              {(months?.count ?? 0) === 0 && (years?.count ?? 0) === 0 && <Row title={t("Months")} tone="calm" sub={t("Every month that has ended is closed.")} />}
              {(months?.count ?? 0) > 0 && (
                <Row
                  title={t("Months")}
                  tone="neutral"
                  amount={isolate(plural(months!.count!, "month"))}
                  sub={placed(t("ended and not closed{p1}. The latest:"), { p1: months!.oldest ? t(", back to {value}", { value: monthName(months!.oldest) }) : "" })}
                >
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
                </Row>
              )}
              {(years?.count ?? 0) > 0 && (
                <Row title={t("Fiscal years")} tone="neutral" amount={isolate(plural(years!.count!, "year"))} sub={t("ended and not closed")}>
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
                </Row>
              )}
            </Group>
          </div>
          {cash && (
            <section className="erp-cash" aria-label={t("Cash and bank")}>
              <h2 className="erp-group">{isolate(t("Cash and bank"))}</h2>
              <p className="erp-q-amount">{isolate(show(cash.amount))}</p>
              <ul className="erp-attn-list">
                {(cash.detail ?? []).map((d) => (
                  <li key={d.id}>
                    <span>{isolate(d.name ?? "")}</span>
                    <span className="num">{isolate(show(d.balance))}</span>
                  </li>
                ))}
              </ul>
            </section>
          )}
        </div>
      </>
    );
  }

  return (
    <div className="erp-page erp-today">
      <PageHead
        title={t("Accounts today")}
        meta={<Meta items={[isolate(new Date(`${asOf}T00:00:00`).toLocaleDateString(undefined, { weekday: "long", day: "numeric", month: "long", year: "numeric" }))]} />}
        actions={
          <>
            <div className="erp-pick">
              <LinkInput to="Company" id="td-company" label={t("Company")} value={company} onChange={(id) => put({ company: id })} />
            </div>
            <label className="erp-pick">
              <span className="sr-only">{t("Look as of")}</span>
              <input className="input" type="date" value={asOf} onChange={(e) => put({ as_of: e.target.value || null })} />
            </label>
          </>
        }
      />
      <AccountsTabs />
      {!company && me ? (
        <StatePanel kind="empty" title={t("Choose a company")}>{t("The overview is for one company at a time.")}</StatePanel>
      ) : (
        body
      )}
    </div>
  );
}
