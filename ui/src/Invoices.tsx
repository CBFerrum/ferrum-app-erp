// ERP's page of the workspace's portal (docs/plan/apps-ui.md §9.1) at /portal/invoices/: a
// customer's own sales invoices, what is still due on each, its lines, and the invoice as the
// workspace prints it. Everything comes from /api/app/selling/portal/invoices…, which answers by
// the address the person signed in with.

import { useInfiniteQuery, useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { Link, useParams } from "react-router";
import { api, download, errorMessage, formatDate, formatDecimal, openPrint } from "@ferrum/client";
import { Badge, Button, EmptyState, ErrorState, PageLoading, statusTone, toast, useShellTitle } from "@ferrum/kit";

const BASE = "/api/app/selling/portal/invoices";

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
  if (inv.is_return) return "Credit note";
  if (Number(inv.outstanding) <= 0) return "Paid";
  if (inv.overdue) return `Overdue since ${formatDate(inv.due_date)}`;
  return inv.due_date ? `Due ${formatDate(inv.due_date)}` : "Due";
}

export function PortalInvoices() {
  useShellTitle("Your invoices");
  const q = useInfiniteQuery({
    queryKey: ["erp", "portal", "invoices"],
    queryFn: ({ pageParam, signal }) => api.get<{ invoices: PortalInvoice[]; next_cursor?: string | null }>(BASE, pageParam ? { cursor: pageParam } : {}, signal),
    initialPageParam: "",
    getNextPageParam: (last) => last.next_cursor || undefined,
  });
  const invoices = q.data?.pages.flatMap((p) => p.invoices) ?? [];
  return (
    <div className="stack">
      <div className="page-head">
        <div>
          <h1 className="page-title">Your invoices</h1>
          <p className="page-sub">What you were billed, and what is still to pay.</p>
        </div>
      </div>
      {q.error ? (
        <ErrorState error={q.error} onRetry={() => void q.refetch()} />
      ) : !q.data ? (
        <PageLoading />
      ) : !invoices.length ? (
        <EmptyState title="No invoices yet" icon="receipt">
          <p className="muted">Invoices made out to you appear here once they are issued.</p>
        </EmptyState>
      ) : (
        <div className="table-wrap" data-ferrum-nocard>
          <table className="table">
            <thead>
              <tr>
                <th scope="col">Invoice</th>
                <th scope="col">Date</th>
                <th scope="col">Status</th>
                <th scope="col" className="num">
                  Total
                </th>
                <th scope="col" className="num">
                  To pay
                </th>
              </tr>
            </thead>
            <tbody>
              {invoices.map((inv) => (
                <tr key={inv.id}>
                  <td>
                    <Link to={`/${inv.id}`}>{inv.number}</Link>
                  </td>
                  <td>{formatDate(inv.date)}</td>
                  <td>
                    <Badge tone={inv.overdue ? "red" : statusTone(inv.status)}>{dueLabel(inv)}</Badge>
                  </td>
                  <td className="num">{amount(inv.total, inv.currency)}</td>
                  <td className="num">{Number(inv.outstanding) > 0 ? amount(inv.outstanding, inv.currency) : "—"}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {q.hasNextPage && (
        <Button loading={q.isFetchingNextPage} onClick={() => void q.fetchNextPage()}>
          Show older invoices
        </Button>
      )}
    </div>
  );
}

export function PortalInvoice() {
  const { id = "" } = useParams();
  const q = useQuery({ queryKey: ["erp", "portal", "invoice", id], queryFn: ({ signal }) => api.get<PortalInvoiceDetail>(`${BASE}/${encodeURIComponent(id)}`, {}, signal) });
  const [busy, setBusy] = useState<"print" | "pdf" | null>(null);
  useShellTitle(q.data?.number ?? "Invoice");
  if (q.error) return <ErrorState error={q.error} onRetry={() => void q.refetch()} />;
  if (!q.data) return <PageLoading />;
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
    <div className="stack">
      <p className="crumbs">
        <Link to="/">Your invoices</Link>
      </p>
      <div className="page-head">
        <div>
          <h1 className="page-title">{inv.number}</h1>
          <p className="page-sub">
            {[inv.company, formatDate(inv.date)].filter(Boolean).join(" · ")}
          </p>
        </div>
        <div className="row">
          <Button loading={busy === "print"} onClick={() => void run("print")}>
            Print
          </Button>
          {inv.pdf && (
            <Button loading={busy === "pdf"} onClick={() => void run("pdf")}>
              Download PDF
            </Button>
          )}
        </div>
      </div>
      <p>
        <Badge tone={inv.overdue ? "red" : statusTone(inv.status)}>{dueLabel(inv)}</Badge>{" "}
        {Number(inv.outstanding) > 0 && <strong>{amount(inv.outstanding, inv.currency)} to pay</strong>}
      </p>
      <div className="table-wrap" data-ferrum-nocard>
        <table className="table">
          <thead>
            <tr>
              <th scope="col">Item</th>
              <th scope="col" className="num">
                Quantity
              </th>
              <th scope="col" className="num">
                Rate
              </th>
              <th scope="col" className="num">
                Amount
              </th>
            </tr>
          </thead>
          <tbody>
            {inv.lines.map((l, i) => (
              <tr key={i}>
                <td>
                  {l.item}
                  {l.description && l.description !== l.item && <div className="faint">{l.description}</div>}
                </td>
                <td className="num">
                  {formatDecimal(l.qty)} {l.unit ?? ""}
                </td>
                <td className="num">{amount(l.rate, inv.currency)}</td>
                <td className="num">{amount(l.amount, inv.currency)}</td>
              </tr>
            ))}
          </tbody>
          <tfoot>
            <tr>
              <th scope="row" colSpan={3} className="num">
                Net total
              </th>
              <td className="num">{amount(inv.net_total, inv.currency)}</td>
            </tr>
            {inv.taxes.map((t, i) => (
              <tr key={i}>
                <th scope="row" colSpan={3} className="num">
                  {t.description ?? "Tax"}
                </th>
                <td className="num">{amount(t.amount, inv.currency)}</td>
              </tr>
            ))}
            <tr>
              <th scope="row" colSpan={3} className="num">
                Total
              </th>
              <td className="num">
                <strong>{amount(inv.total, inv.currency)}</strong>
              </td>
            </tr>
          </tfoot>
        </table>
      </div>
    </div>
  );
}
