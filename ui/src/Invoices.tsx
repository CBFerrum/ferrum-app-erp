// ERP's page of the workspace's portal (docs/plan/apps-ui.md §9.1) at /portal/invoices/: a
// customer's own sales invoices, what is still due on each, its lines, and the invoice as the
// workspace prints it. Everything comes from /api/app/selling/portal/invoices…, which answers by
// the address the person signed in with.

import { useInfiniteQuery, useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { Link, useParams } from "react-router";
import { api, download, errorMessage, formatDate, formatDecimal, openPrint, t as tr, isolate } from "@ferrum/client";
import { Badge, Button, statusTone, toast, useShellTitle, type Tone, StatePanel } from "@ferrum/kit";
import { Loading, LoadFailed } from "./states";

const BASE = "/api/app/selling/portal/invoices";

/** The colour of each stored status of a sales invoice (the options of its `status` field in packages/accounting). */
export const INVOICE_TONES: Readonly<Record<string, Tone>> = {
  "Draft": "neutral",
  "Unpaid": "amber",
  "Partly Paid": "amber",
  "Paid": "green",
  "Credit": "neutral",
  "Cancelled": "red",
};

export interface PortalInvoice {
  id: string;
  number?: string | null;
  date?: string | null;
  due_date?: string | null;
  status?: string | null;
  is_return?: boolean;
  customer?: string | null;
  currency?: string | null;
  total: string;
  outstanding: string;
  overdue: boolean;
}

interface InvoiceLine {
  item?: string | null;
  description?: string | null;
  qty: string;
  unit?: string | null;
  rate: string;
  amount: string;
}

interface PortalInvoiceDetail extends PortalInvoice {
  company?: string | null;
  lines: InvoiceLine[];
  taxes: { description?: string | null; amount: string }[];
  net_total: string;
  total_taxes: string;
  grand_total: string;
  pdf: boolean;
}

/** An amount with its currency, as the invoice shows it. */
export function amount(value: string, currency?: string | null): string {
  const n = formatDecimal(value, 2);
  return currency ? `${currency} ${n}` : n;
}

/** What is still to pay, in words a customer reads. */
export function dueLabel(inv: Pick<PortalInvoice, "outstanding" | "overdue" | "due_date" | "is_return">): string {
  if (inv.is_return) return tr("Credit note");
  if (Number(inv.outstanding) <= 0) return "Paid";
  if (inv.overdue) return tr("Overdue since {due_date}", { due_date: formatDate(inv.due_date) });
  return inv.due_date ? tr("Due {due_date}", { due_date: formatDate(inv.due_date) }) : "Due";
}

export function PortalInvoices() {
  useShellTitle(tr("Your invoices"));
  const q = useInfiniteQuery({
    queryKey: ["erp", "portal", "invoices"],
    queryFn: ({ pageParam, signal }) => api.get<{ invoices: PortalInvoice[]; next_cursor?: string | null }>(BASE, pageParam ? { cursor: pageParam } : {}, signal),
    initialPageParam: "",
    getNextPageParam: (last) => last.next_cursor || undefined,
  });
  const invoices = q.data?.pages.flatMap((p) => p.invoices) ?? [];
  return (
    <div className="stack erp-portal">
      <div className="page-head">
        <div>
          <h1 className="page-title">{tr("Your invoices")}</h1>
          <p className="page-sub">{tr("What you were billed, and what is still to pay.")}</p>
        </div>
      </div>
      {q.error ? (
        <LoadFailed error={q.error} onRetry={() => void q.refetch()} />
      ) : !q.data ? (
        <Loading />
      ) : !invoices.length ? (
        <StatePanel kind="empty" title={tr("No invoices yet")}>{tr("Invoices made out to you appear here once they are issued.")}</StatePanel>
      ) : (
        <div className="table-wrap" data-ferrum-nocard>
          <table className="table">
            <thead>
              <tr>
                <th scope="col">{tr("Invoice")}</th>
                <th scope="col">{tr("Date")}</th>
                <th scope="col">{tr("Status")}</th>
                <th scope="col" className="num">
                  {tr("Total")}
                </th>
                <th scope="col" className="num">
                  {tr("To pay")}
                </th>
              </tr>
            </thead>
            <tbody>
              {invoices.map((inv) => (
                <tr key={inv.id}>
                  <td>
                    <Link to={`/${inv.id}`}>{isolate(inv.number ?? "")}</Link>
                  </td>
                  <td>{isolate(formatDate(inv.date))}</td>
                  <td>
                    <Badge tone={inv.overdue ? "red" : statusTone(inv.status, INVOICE_TONES)}>{isolate(dueLabel(inv))}</Badge>
                  </td>
                  <td className="num">{isolate(amount(inv.total, inv.currency))}</td>
                  <td className={`num${inv.overdue && Number(inv.outstanding) > 0 ? " erp-due" : ""}`}>{Number(inv.outstanding) > 0 ? amount(inv.outstanding, inv.currency) : "—"}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {q.hasNextPage && (
        <Button loading={q.isFetchingNextPage} onClick={() => void q.fetchNextPage()}>
          {tr("Show older invoices")}
        </Button>
      )}
    </div>
  );
}

export function PortalInvoice() {
  const { id = "" } = useParams();
  const q = useQuery({ queryKey: ["erp", "portal", "invoice", id], queryFn: ({ signal }) => api.get<PortalInvoiceDetail>(`${BASE}/${encodeURIComponent(id)}`, {}, signal) });
  const [busy, setBusy] = useState<"print" | "pdf" | null>(null);
  useShellTitle(q.data?.number ?? tr("Invoice"));
  if (q.error) return <LoadFailed error={q.error} onRetry={() => void q.refetch()} />;
  if (!q.data) return <Loading shape="reader" />;
  const inv = q.data;
  const run = async (what: "print" | "pdf") => {
    setBusy(what);
    try {
      if (what === "print") await openPrint(`${BASE}/${encodeURIComponent(inv.id)}/print`);
      else await download(`${BASE}/${encodeURIComponent(inv.id)}/print`, `${inv.number ?? "invoice"}.pdf`, { format: "pdf" });
    } catch (err) {
      toast(errorMessage(err), "error");
    } finally {
      setBusy(null);
    }
  };
  return (
    <div className="stack erp-portal">
      <p className="crumbs">
        <Link to="/">{tr("Your invoices")}</Link>
      </p>
      <div className="page-head">
        <div>
          <h1 className="page-title">{isolate(inv.number ?? "")}</h1>
          <p className="page-sub">
            {isolate([inv.company, formatDate(inv.date)].filter(Boolean).join(" · "))}
          </p>
        </div>
        <div className="page-actions">
          <Button loading={busy === "print"} onClick={() => void run("print")}>
            {tr("Print")}
          </Button>
          {inv.pdf && (
            <Button loading={busy === "pdf"} onClick={() => void run("pdf")}>
              {tr("Download PDF")}
            </Button>
          )}
        </div>
      </div>
      <div className="erp-owed">
        {Number(inv.outstanding) > 0 && (
          <p className="erp-owed-amount">
            {isolate(amount(inv.outstanding, inv.currency))} <span>{tr("to pay")}</span>
          </p>
        )}
        <Badge tone={inv.overdue ? "red" : statusTone(inv.status, INVOICE_TONES)}>{isolate(dueLabel(inv))}</Badge>
      </div>
      <div className="table-wrap" data-ferrum-nocard>
        <table className="table">
          <thead>
            <tr>
              <th scope="col">{tr("Item")}</th>
              <th scope="col" className="num">
                {tr("Quantity")}
              </th>
              <th scope="col" className="num">
                {tr("Rate")}
              </th>
              <th scope="col" className="num">
                {tr("Amount")}
              </th>
            </tr>
          </thead>
          <tbody>
            {inv.lines.map((l, i) => (
              <tr key={i}>
                <td>
                  {isolate(l.item ?? "")}
                  {l.description && l.description !== l.item && <div className="faint">{isolate(l.description)}</div>}
                </td>
                <td className="num">
                  {isolate(formatDecimal(l.qty))} {l.unit ?? ""}
                </td>
                <td className="num">{isolate(amount(l.rate, inv.currency))}</td>
                <td className="num">{isolate(amount(l.amount, inv.currency))}</td>
              </tr>
            ))}
          </tbody>
          <tfoot>
            <tr>
              <th scope="row" colSpan={3} className="num">
                {tr("Net total")}
              </th>
              <td className="num">{isolate(amount(inv.net_total, inv.currency))}</td>
            </tr>
            {inv.taxes.map((t, i) => (
              <tr key={i}>
                <th scope="row" colSpan={3} className="num">
                  {t.description ?? tr("Tax")}
                </th>
                <td className="num">{isolate(amount(t.amount, inv.currency))}</td>
              </tr>
            ))}
            <tr>
              <th scope="row" colSpan={3} className="num">
                {tr("Total")}
              </th>
              <td className="num">
                <strong>{isolate(amount(inv.total, inv.currency))}</strong>
              </td>
            </tr>
          </tfoot>
        </table>
      </div>
    </div>
  );
}
