// ERP's page of the workspace's portal for suppliers (docs/plan/apps-ui.md §9.1) at /portal/orders/:
// the requests for quotation waiting for their answer (where Procurement is installed), what was
// ordered from them, how much of each order was received and billed, the order as the workspace
// prints it, and the bills ERP holds from them with what is still owed. Everything comes from
// /api/app/buying/portal/… and /api/app/procurement/portal/rfqs, which answer by the address the
// person signed in with.

import { useInfiniteQuery, useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { Link, useParams } from "react-router";
import { api, download, errorMessage, formatDate, formatDecimal, openPrint, t as tr, isolate } from "@ferrum/client";
import { Badge, Button, EmptyState, ErrorState, PageLoading, statusTone, toast, useShellTitle } from "@ferrum/kit";
import { amount } from "./Invoices";

const BASE = "/api/app/buying/portal";
const RFQS = "/api/app/procurement/portal/rfqs";

export interface PortalRfq {
  id: string;
  number?: string | null;
  due_by?: string | null;
  company?: string | null;
  items: { item?: string | null; qty: string; unit?: string | null }[];
  /** The page where the supplier answers, as it was e-mailed to them. */
  answer?: string | null;
}

/** What a request asks for, in a line. */
export function asked(r: Pick<PortalRfq, "items">): string {
  return r.items.map((i) => `${formatDecimal(i.qty)}${i.unit ? ` ${i.unit}` : ""} ${i.item ?? ""}`.trim()).join(", ");
}

export interface PortalOrder {
  id: string;
  number?: string | null;
  date?: string | null;
  required_by?: string | null;
  status?: string | null;
  supplier?: string | null;
  currency?: string | null;
  total: string;
  received: string;
  billed: string;
}

interface OrderLine {
  item?: string | null;
  description?: string | null;
  qty: string;
  received: string;
  unit?: string | null;
  rate: string;
  amount: string;
  required_by?: string | null;
}

interface PortalOrderDetail extends PortalOrder {
  company?: string | null;
  lines: OrderLine[];
  taxes: { description?: string | null; amount: string }[];
  net_total: string;
  grand_total: string;
  pdf: boolean;
}

export interface PortalBill {
  id: string;
  bill_no?: string | null;
  bill_date?: string | null;
  due_date?: string | null;
  status?: string | null;
  is_return?: boolean;
  currency?: string | null;
  total: string;
  owed: string;
  overdue: boolean;
}

/** How far an order has come, in words a supplier reads. */
export function progress(o: Pick<PortalOrder, "received" | "billed" | "status">): string {
  if (o.status === "Closed") return "Closed";
  const received = Number(o.received);
  const billed = Number(o.billed);
  if (received >= 100 && billed >= 100) return "Received and billed";
  if (received >= 100) return "Received, to bill";
  if (billed >= 100) return received > 0 ? `Billed, ${formatDecimal(o.received, 0)}% received` : "Billed, to deliver";
  if (received > 0) return `${formatDecimal(o.received, 0)}% received`;
  return "To deliver";
}

/** What is still owed on a bill, in words a supplier reads. */
export function owedLabel(b: Pick<PortalBill, "owed" | "overdue" | "due_date" | "is_return">): string {
  if (b.is_return) return "Debit note";
  if (Number(b.owed) <= 0) return "Paid";
  if (b.overdue) return `Overdue since ${formatDate(b.due_date)}`;
  return b.due_date ? `Due ${formatDate(b.due_date)}` : "To be paid";
}

export function PortalOrders() {
  useShellTitle(tr("Your orders"));
  const orders = useInfiniteQuery({
    queryKey: ["erp", "portal", "orders"],
    queryFn: ({ pageParam, signal }) => api.get<{ orders: PortalOrder[]; next_cursor?: string | null }>(`${BASE}/orders`, pageParam ? { cursor: pageParam } : {}, signal),
    initialPageParam: "",
    getNextPageParam: (last) => last.next_cursor || undefined,
  });
  const bills = useInfiniteQuery({
    queryKey: ["erp", "portal", "bills"],
    queryFn: ({ pageParam, signal }) => api.get<{ bills: PortalBill[]; next_cursor?: string | null }>(`${BASE}/bills`, pageParam ? { cursor: pageParam } : {}, signal),
    initialPageParam: "",
    getNextPageParam: (last) => last.next_cursor || undefined,
  });
  // Only where Procurement is installed; elsewhere there is nothing to ask for.
  const rfqs = useQuery({ queryKey: ["erp", "portal", "rfqs"], queryFn: ({ signal }) => api.get<{ rfqs: PortalRfq[] }>(RFQS, {}, signal) });
  const waiting = rfqs.data?.rfqs ?? [];
  const orderRows = orders.data?.pages.flatMap((p) => p.orders) ?? [];
  const billRows = bills.data?.pages.flatMap((p) => p.bills) ?? [];
  return (
    <div className="stack">
      <div className="page-head">
        <div>
          <h1 className="page-title">{tr("Your orders")}</h1>
          <p className="page-sub">{tr("What was ordered from you, how much of it was received, and what you are still owed.")}</p>
        </div>
      </div>
      {waiting.length > 0 && (
        <section className="stack" aria-label={tr("Requests for quotation")}>
          <h2 className="section-title">{tr("Waiting for your quotation")}</h2>
          <ul className="stack">
            {waiting.map((r) => (
              <li key={`${r.id}-${r.answer ?? ""}`} className="row" style={{ justifyContent: "space-between", flexWrap: "wrap" }}>
                <span>
                  <strong>{isolate(r.number ?? "")}</strong> · {isolate(asked(r))}
                  {r.due_by && <span className="faint"> {tr("· due by")} {isolate(formatDate(r.due_by))}</span>}
                </span>
                {r.answer ? <a href={r.answer}>{tr("Answer it")}</a> : <span className="faint">{tr("Ask")} {r.company ?? tr("them")} {tr("for a new link")}</span>}
              </li>
            ))}
          </ul>
        </section>
      )}
      {orders.error ? (
        <ErrorState error={orders.error} onRetry={() => void orders.refetch()} />
      ) : !orders.data ? (
        <PageLoading />
      ) : !orderRows.length ? (
        <EmptyState title={tr("No orders yet")} icon="receipt">
          <p className="muted">{tr("Orders placed with you appear here once they are sent.")}</p>
        </EmptyState>
      ) : (
        <section className="stack" aria-label={tr("Orders")}>
          <div className="table-wrap" data-ferrum-nocard>
            <table className="table">
              <thead>
                <tr>
                  <th scope="col">{tr("Order")}</th>
                  <th scope="col">{tr("Date")}</th>
                  <th scope="col">{tr("Required by")}</th>
                  <th scope="col">{tr("Where it stands")}</th>
                  <th scope="col" className="num">
                    {tr("Total")}
                  </th>
                </tr>
              </thead>
              <tbody>
                {orderRows.map((o) => (
                  <tr key={o.id}>
                    <td>
                      <Link to={`/${o.id}`}>{isolate(o.number ?? "")}</Link>
                    </td>
                    <td>{isolate(formatDate(o.date))}</td>
                    <td>{o.required_by ? formatDate(o.required_by) : "—"}</td>
                    <td>
                      <Badge tone={statusTone(o.status)}>{isolate(progress(o))}</Badge>
                    </td>
                    <td className="num">{isolate(amount(o.total, o.currency))}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          {orders.hasNextPage && (
            <Button loading={orders.isFetchingNextPage} onClick={() => void orders.fetchNextPage()}>
              {tr("Show older orders")}
            </Button>
          )}
        </section>
      )}
      <h2 className="section-title">{tr("Your bills")}</h2>
      {bills.error ? (
        <ErrorState error={bills.error} onRetry={() => void bills.refetch()} />
      ) : !bills.data ? (
        <PageLoading />
      ) : !billRows.length ? (
        <p className="muted">{tr("No bills of yours are recorded yet.")}</p>
      ) : (
        <section className="stack" aria-label={tr("Bills")}>
          <div className="table-wrap" data-ferrum-nocard>
            <table className="table">
              <thead>
                <tr>
                  <th scope="col">{tr("Your invoice")}</th>
                  <th scope="col">{tr("Date")}</th>
                  <th scope="col">{tr("Status")}</th>
                  <th scope="col" className="num">
                    {tr("Total")}
                  </th>
                  <th scope="col" className="num">
                    {tr("Still owed")}
                  </th>
                </tr>
              </thead>
              <tbody>
                {billRows.map((b) => (
                  <tr key={b.id}>
                    <td>{isolate(b.bill_no ?? "")}</td>
                    <td>{isolate(formatDate(b.bill_date))}</td>
                    <td>
                      <Badge tone={b.overdue ? "red" : statusTone(b.status)}>{isolate(owedLabel(b))}</Badge>
                    </td>
                    <td className="num">{isolate(amount(b.total, b.currency))}</td>
                    <td className="num">{Number(b.owed) > 0 ? amount(b.owed, b.currency) : "—"}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          {bills.hasNextPage && (
            <Button loading={bills.isFetchingNextPage} onClick={() => void bills.fetchNextPage()}>
              {tr("Show older bills")}
            </Button>
          )}
        </section>
      )}
    </div>
  );
}

export function PortalOrderPage() {
  const { id = "" } = useParams();
  const q = useQuery({ queryKey: ["erp", "portal", "order", id], queryFn: ({ signal }) => api.get<PortalOrderDetail>(`${BASE}/orders/${encodeURIComponent(id)}`, {}, signal) });
  const [busy, setBusy] = useState<"print" | "pdf" | null>(null);
  useShellTitle(q.data?.number ?? tr("Order"));
  if (q.error) return <ErrorState error={q.error} onRetry={() => void q.refetch()} />;
  if (!q.data) return <PageLoading />;
  const o = q.data;
  const run = async (what: "print" | "pdf") => {
    setBusy(what);
    try {
      if (what === "print") await openPrint(`${BASE}/orders/${encodeURIComponent(o.id)}/print`);
      else await download(`${BASE}/orders/${encodeURIComponent(o.id)}/print`, `${o.number ?? "order"}.pdf`, { format: "pdf" });
    } catch (err) {
      toast(errorMessage(err), "error");
    } finally {
      setBusy(null);
    }
  };
  return (
    <div className="stack">
      <p className="crumbs">
        <Link to="/">{tr("Your orders")}</Link>
      </p>
      <div className="page-head">
        <div>
          <h1 className="page-title">{isolate(o.number ?? "")}</h1>
          <p className="page-sub">
            {isolate([o.company, formatDate(o.date), o.required_by ? `required by ${formatDate(o.required_by)}` : null].filter(Boolean).join(" · "))}
          </p>
        </div>
        <div className="row">
          <Button loading={busy === "print"} onClick={() => void run("print")}>
            {tr("Print")}
          </Button>
          {o.pdf && (
            <Button loading={busy === "pdf"} onClick={() => void run("pdf")}>
              {tr("Download PDF")}
            </Button>
          )}
        </div>
      </div>
      <p>
        <Badge tone={statusTone(o.status)}>{isolate(progress(o))}</Badge>
      </p>
      <div className="table-wrap" data-ferrum-nocard>
        <table className="table">
          <thead>
            <tr>
              <th scope="col">{tr("Item")}</th>
              <th scope="col" className="num">
                {tr("Ordered")}
              </th>
              <th scope="col" className="num">
                {tr("Received")}
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
            {o.lines.map((l, i) => (
              <tr key={i}>
                <td>
                  {isolate(l.item ?? "")}
                  {l.description && l.description !== l.item && <div className="faint">{isolate(l.description)}</div>}
                  {l.required_by && l.required_by !== o.required_by && <div className="faint">{tr("Required by")} {isolate(formatDate(l.required_by))}</div>}
                </td>
                <td className="num">
                  {isolate(formatDecimal(l.qty))} {l.unit ?? ""}
                </td>
                <td className="num">{isolate(formatDecimal(l.received))}</td>
                <td className="num">{isolate(amount(l.rate, o.currency))}</td>
                <td className="num">{isolate(amount(l.amount, o.currency))}</td>
              </tr>
            ))}
          </tbody>
          <tfoot>
            <tr>
              <th scope="row" colSpan={4} className="num">
                {tr("Net total")}
              </th>
              <td className="num">{isolate(amount(o.net_total, o.currency))}</td>
            </tr>
            {o.taxes.map((t, i) => (
              <tr key={i}>
                <th scope="row" colSpan={4} className="num">
                  {t.description ?? tr("Tax")}
                </th>
                <td className="num">{isolate(amount(t.amount, o.currency))}</td>
              </tr>
            ))}
            <tr>
              <th scope="row" colSpan={4} className="num">
                {tr("Total")}
              </th>
              <td className="num">
                <strong>{isolate(amount(o.total, o.currency))}</strong>
              </td>
            </tr>
          </tfoot>
        </table>
      </div>
    </div>
  );
}
