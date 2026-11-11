// Applying a customer's or supplier's unapplied money to their open invoices, in one sitting: the
// money there is (payments, credit notes, journal credits) on one side, what is owed on the other,
// an amount against each invoice (oldest first with one button), and what is left over on both
// sides always in view. Confirming shows the pairings with the exchange gain or loss the server
// works out for each, and then posts every one of them or none. The rules about what may be paired
// are the server's; this screen only keeps the sums.

import { useQuery, useQueryClient } from "@tanstack/react-query";
import { type FormEvent, useCallback, useMemo, useState } from "react";
import { Link, useSearchParams } from "react-router";
import { api, docPath, errorMessage, formatDate, todayIso, t as tr, isolate } from "@ferrum/client";
import { Alert, Button, Dialog, Field, Icon, toast, useLiveEntity, useShellTitle, StatePanel } from "@ferrum/kit";
import { Loading, LoadFailed } from "./states";
import { LinkInput, useMe } from "@ferrum/records";
import { currencies, documentBody, dueWords, fill, fromReport, kindLabel, type Open, pairOff, postingDate, ready, split, type Tally, tally } from "./allocation";
import { cmp, min, sub, sum } from "./decimal";
import { show } from "./money";
import { Ctl, Meta, PageHead } from "./chrome";
import { AccountsTabs } from "./Tabs";

type PartyType = "Customer" | "Supplier";

interface Report {
  rows: Parameters<typeof fromReport>[0];
}

interface Preview {
  allocations: { credit_type?: string; credit_number?: string | null; invoice_number?: string | null; allocated_amount: string; exchange_difference: string }[];
  total_allocated: string;
  exchange_difference: string;
}

const ROOT = ["erp", "allocate"] as const;

// ───────────────────────────── the sums on screen ─────────────────────────────

function Summary({ t, currency, onFill, onClear, onReview, canFill }: { t: Tally; currency: string; onFill: () => void; onClear: () => void; onReview: () => void; canFill: boolean }) {
  return (
    <div className="erp-summary" role="status">
      <dl className="erp-sums">
        <div>
          <dt>{tr("Money to apply")}</dt>
          <dd>{isolate(show(t.available))}</dd>
        </div>
        <div>
          <dt>{tr("Applying")}</dt>
          <dd>{isolate(show(t.applied))}</dd>
        </div>
        <div data-tone={cmp(t.left, "0") > 0 ? "left" : undefined}>
          <dt>{tr("Left over")}</dt>
          <dd>{isolate(show(t.left))}</dd>
        </div>
      </dl>
      <span className="faint">{isolate(currency)}</span>
      {t.over && <span className="erp-negative">{isolate(t.over)}</span>}
      <span className="spacer" />
      <Button onClick={onFill} disabled={!canFill}>
        {tr("Oldest invoices first")}
      </Button>
      <Button variant="ghost" onClick={onClear} disabled={cmp(t.applied, "0") === 0}>
        {tr("Clear")}
      </Button>
      <Button variant="primary" onClick={onReview} disabled={!ready(t)}>
        {tr("Review")}
      </Button>
    </div>
  );
}

function MoneyTable({ credits, left, used, precision, onToggle }: { credits: Open[]; left: Record<string, string>; used: Set<string>; precision: number; onToggle: (id: string, on: boolean) => void }) {
  return (
    <section className="erp-card" aria-labelledby="al-money">
      <h2 id="al-money" className="erp-card-title">
        {tr("Money to apply")}
      </h2>
      <div className="table-wrap">
        <table className="table">
          <thead>
            <tr>
              <th>
                <span className="sr-only">{tr("Use")}</span>
              </th>
              <th>{tr("Document")}</th>
              <th className="num">{tr("Available")}</th>
              <th className="num">{tr("Left over")}</th>
            </tr>
          </thead>
          <tbody>
            {credits.map((c) => (
              <tr key={c.id} data-off={!used.has(c.id) || undefined}>
                <td>
                  <input type="checkbox" aria-label={tr("Use {c} {value}", { c: kindLabel(c), value: c.number ?? "" })} checked={used.has(c.id)} onChange={(e) => onToggle(c.id, e.target.checked)} />
                </td>
                <td>
                  <DocLink item={c} />
                  <div className="faint">
                    {isolate(kindLabel(c))} · {isolate(formatDate(c.date))}
                  </div>
                </td>
                <td className="num">{isolate(show(sub("0", c.outstanding), precision))}</td>
                <td className="num">{used.has(c.id) ? show(left[c.id], precision) : <span className="faint">{tr("not used")}</span>}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}

function DocLink({ item }: { item: Open }) {
  const label = item.number ?? kindLabel(item);
  // A journal's open items are its lines: the journal itself is found from its list.
  if (item.kind === "JournalEntry") return <span className="cell-title">{isolate(label)}</span>;
  return (
    <Link className="cell-title" to={`/e/${item.kind}/${item.id}`}>
      {isolate(label)}
    </Link>
  );
}

function InvoiceTable({ invoices, amounts, t, available, onAmount, today }: { invoices: Open[]; amounts: Record<string, string>; t: Tally; available: string; onAmount: (id: string, v: string) => void; today: string }) {
  // "Full" asks for as much of the invoice as the money not given to the other invoices can pay.
  const room = (id: string) => sub(available, sum(Object.entries(amounts).filter(([k]) => k !== id).map(([, v]) => v)));
  return (
    <section className="erp-card" aria-labelledby="al-owed">
      <h2 id="al-owed" className="erp-card-title">
        {tr("Open invoices")}
      </h2>
      <div className="table-wrap">
        <table className="table">
          <thead>
            <tr>
              <th>{tr("Invoice")}</th>
              <th>{tr("Due")}</th>
              <th className="num">{tr("Owed")}</th>
              <th className="num">{tr("Apply")}</th>
              <th className="num">{tr("Left owed")}</th>
            </tr>
          </thead>
          <tbody>
            {invoices.map((inv) => {
              const late = dueWords(inv.due ?? inv.date, today);
              const full = min(inv.outstanding, cmp(room(inv.id), "0") > 0 ? room(inv.id) : "0");
              return (
                <tr key={inv.id}>
                  <td>
                    <DocLink item={inv} />
                    <div className="faint">{isolate(formatDate(inv.date))}</div>
                  </td>
                  <td>
                    {inv.due ? formatDate(inv.due) : ""}
                    <div className={late.includes("overdue") ? "erp-negative" : "faint"}>{isolate(late)}</div>
                  </td>
                  <td className="num">{isolate(show(inv.outstanding))}</td>
                  <td className="num erp-apply">
                    <input
                      className="input num"
                      inputMode="decimal"
                      aria-label={tr("Amount to apply to {value}", { value: inv.number ?? tr("the invoice") })}
                      aria-invalid={t.errors[inv.id] ? true : undefined}
                      placeholder="0"
                      value={amounts[inv.id] ?? ""}
                      onChange={(e) => onAmount(inv.id, e.target.value)}
                    />
                    <Button size="sm" variant="ghost" disabled={cmp(full, "0") <= 0} onClick={() => onAmount(inv.id, full)} aria-label={tr("Apply as much as possible to {value}", { value: inv.number ?? tr("the invoice") })}>
                      {tr("Full")}
                    </Button>
                    {t.errors[inv.id] && <span className="field-error">{isolate(t.errors[inv.id])}</span>}
                  </td>
                  <td className="num">{isolate(show(t.invoiceLeft[inv.id]))}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </section>
  );
}

// ───────────────────────────── confirming ─────────────────────────────

function Review({ company, partyType, party, pairings, byId, minDate, onClose, onDone }: { company: string; partyType: PartyType; party: string; pairings: ReturnType<typeof pairOff>["pairings"]; byId: Map<string, Open>; minDate: string; onClose: () => void; onDone: () => void }) {
  const [date, setDate] = useState(minDate);
  const [remarks, setRemarks] = useState("");
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const body = useMemo(() => documentBody({ company, partyType, party, date, pairings, remarks: remarks.trim() }), [company, partyType, party, date, pairings, remarks]);
  // The server works out each pairing's documents and exchange difference before anything is saved.
  const seen = useQuery({
    queryKey: [...ROOT, "preview", JSON.stringify(body)],
    queryFn: ({ signal }) => api.post<Preview>("/api/v1/PaymentAllocation/_calc", body, { signal }),
  });
  const p = seen.data;
  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setProblem(null);
    let made: { id: string; version: number } | null = null;
    try {
      made = await api.post<{ id: string; version: number }>("/api/v1/PaymentAllocation", body);
      const done = await api.post<{ number?: string }>(docPath("PaymentAllocation", made.id, "submit"), {}, { ifMatch: made.version });
      toast(tr("Applied {value}{value2}", { value: show(p?.total_allocated), value2: done.number ? ` (${done.number})` : "" }));
      onDone();
    } catch (err) {
      // A draft that could not be posted is not left behind.
      if (made) await api.del(docPath("PaymentAllocation", made.id)).catch(() => undefined);
      setProblem(errorMessage(err));
      setBusy(false);
    }
  };
  const gain = p?.exchange_difference ?? "0";
  return (
    <Dialog
      open
      wide
      onClose={onClose}
      title={tr("Review before applying")}
      footer={
        <>
          <Button onClick={onClose}>{tr("Back")}</Button>
          <Button variant="primary" type="submit" form="al-confirm" loading={busy} disabled={!p}>
            {tr("Apply")} {p ? show(p.total_allocated) : ""}
          </Button>
        </>
      }
    >
      <form id="al-confirm" className="stack erp-tight" onSubmit={submit}>
        {problem && <Alert tone="error">{isolate(problem)}</Alert>}
        {seen.error ? (
          <Alert tone="error">{isolate(errorMessage(seen.error))}</Alert>
        ) : (
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th>{tr("Money")}</th>
                  <th>{tr("Applied to")}</th>
                  <th className="num">{tr("Amount")}</th>
                  <th className="num">{tr("Exchange gain (loss)")}</th>
                </tr>
              </thead>
              <tbody>
                {pairings.map((pair, i) => (
                  <tr key={`${pair.credit}-${pair.invoice}`}>
                    <td>
                      {p?.allocations[i]?.credit_number ?? byId.get(pair.credit)?.number}
                      <div className="faint">{isolate(p?.allocations[i]?.credit_type ?? "")}</div>
                    </td>
                    <td>{p?.allocations[i]?.invoice_number ?? byId.get(pair.invoice)?.number}</td>
                    <td className="num">{isolate(show(pair.amount))}</td>
                    <td className="num">{p ? (cmp(p.allocations[i]?.exchange_difference ?? "0", "0") === 0 ? <span className="faint">{tr("none")}</span> : show(p.allocations[i].exchange_difference)) : "…"}</td>
                  </tr>
                ))}
              </tbody>
              {p && cmp(gain, "0") !== 0 && (
                <tfoot>
                  <tr>
                    <td colSpan={3}>{(cmp(gain, "0") > 0 ? tr("Exchange gain, in the company's currency") : tr("Exchange loss, in the company's currency"))}</td>
                    <td className="num">{isolate(show(gain))}</td>
                  </tr>
                </tfoot>
              )}
            </table>
          </div>
        )}
        <div className="erp-two">
          <Field label={tr("Dated")} htmlFor="al-date" hint={tr("Not before the money or the invoices it joins.")}>
            <input id="al-date" className="input" type="date" required min={minDate} value={date} onChange={(e) => setDate(e.target.value)} />
          </Field>
          <Field label={tr("Note")} htmlFor="al-note">
            <input id="al-note" className="input" value={remarks} onChange={(e) => setRemarks(e.target.value)} />
          </Field>
        </div>
      </form>
    </Dialog>
  );
}

// ───────────────────────────── the screen ─────────────────────────────

function Body({ items, company, partyType, party, onApplied }: { items: Open[]; company: string; partyType: PartyType; party: string; onApplied: () => void }) {
  const [sp, setSp] = useSearchParams();
  const all = useMemo(() => currencies(items), [items]);
  const currency = all.includes(sp.get("currency") ?? "") ? sp.get("currency")! : (all[0] ?? "");
  const { credits, invoices } = useMemo(() => split(items, currency), [items, currency]);
  const [amounts, setAmounts] = useState<Record<string, string>>({});
  const [off, setOff] = useState<Set<string>>(new Set());
  const [reviewing, setReviewing] = useState(false);
  const today = todayIso();

  const usable = useMemo(() => credits.filter((c) => !off.has(c.id)), [credits, off]);
  const t = useMemo(() => tally(usable, invoices, amounts), [usable, invoices, amounts]);
  const { pairings } = useMemo(() => pairOff(usable, invoices, amounts), [usable, invoices, amounts]);
  const byId = useMemo(() => new Map(items.map((i) => [i.id, i])), [items]);
  const minDate = postingDate(today, pairings.flatMap((p) => [byId.get(p.credit)?.date, byId.get(p.invoice)?.date]).filter((d): d is string => !!d));

  const reset = () => setAmounts({});
  const setAmount = (id: string, v: string) => setAmounts((a) => (v === "" ? Object.fromEntries(Object.entries(a).filter(([k]) => k !== id)) : { ...a, [id]: v }));

  if (!credits.length || !invoices.length) {
    return (
      <StatePanel
        kind="empty"
        title={!credits.length ? tr("No money waiting to be applied") : tr("No open invoices to apply it to")}
        action={
          !credits.length && (
            <Link className="btn btn-default btn-md" to="/e/PaymentEntry/new">
              <Icon name="plus" />
              {tr("Record a payment")}
            </Link>
          )
        }
      >
        {!credits.length
          ? tr("{value}. A payment received without an invoice named, a credit note or a journal credit shows here once it is recorded.", { value: items.length ? tr("Everything this party owes is still to be paid") : tr("Nothing is open for this party") })
          : tr("The money is waiting, with nothing yet to apply it to.")}
      </StatePanel>
    );
  }
  return (
    <>
      {all.length > 1 && (
        <div className="segmented" role="group" aria-label={tr("Currency")}>
          {all.map((c) => (
            <button
              key={c}
              type="button"
              aria-pressed={c === currency}
              onClick={() => {
                reset();
                setOff(new Set());
                setSp((prev) => new URLSearchParams({ ...Object.fromEntries(prev), currency: c }), { replace: true });
              }}
            >
              {isolate(c)}
            </button>
          ))}
        </div>
      )}
      <Summary
        t={t}
        currency={currency}
        canFill={cmp(t.available, "0") > 0}
        onFill={() => setAmounts(fill(usable, invoices))}
        onClear={reset}
        onReview={() => setReviewing(true)}
      />
      <div className="erp-pair">
        <MoneyTable
          credits={credits}
          left={t.creditLeft}
          used={new Set(usable.map((c) => c.id))}
          precision={2}
          onToggle={(id, on) =>
            setOff((o) => {
              const next = new Set(o);
              if (on) next.delete(id);
              else next.add(id);
              return next;
            })
          }
        />
        <InvoiceTable invoices={invoices} amounts={amounts} t={t} available={t.available} onAmount={setAmount} today={today} />
      </div>
      {reviewing && (
        <Review
          company={company}
          partyType={partyType}
          party={party}
          pairings={pairings}
          byId={byId}
          minDate={minDate}
          onClose={() => setReviewing(false)}
          onDone={() => {
            setReviewing(false);
            reset();
            onApplied();
          }}
        />
      )}
      <p className="faint">{tr("Untick a payment to keep it for later. Oldest invoices first pays the earliest due dates with the oldest money.")}</p>
    </>
  );
}

export function Allocate() {
  useShellTitle(tr("Apply payments"));
  const qc = useQueryClient();
  const me = useMe().data;
  const [sp, setSp] = useSearchParams();
  const partyType: PartyType = sp.get("type") === "Supplier" ? "Supplier" : "Customer";
  const party = sp.get("party");
  const company = sp.get("company") ?? me?.default_company ?? null;
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
  const q = useQuery({
    queryKey: [...ROOT, "open", partyType, party, company],
    enabled: !!party && !!company,
    queryFn: ({ signal }) =>
      api.get<Report>(`/api/reports/${partyType === "Customer" ? "receivables" : "payables"}`, { company, [partyType.toLowerCase()]: party, as_of: todayIso() }, signal),
  });
  const refresh = useCallback(() => void qc.invalidateQueries({ queryKey: ROOT }), [qc]);
  useLiveEntity("PaymentAllocation", refresh);
  useLiveEntity("PaymentEntry", refresh);
  useLiveEntity(partyType === "Customer" ? "SalesInvoice" : "PurchaseInvoice", refresh);
  const items = useMemo(() => (q.data ? fromReport(q.data.rows) : []), [q.data]);

  let body;
  if (!party) {
    body = (
      <StatePanel kind="empty" title={tr("Choose a {partyType}", { partyType: partyType.toLowerCase() })}>{tr("Their payments, credit notes and journal credits that are not applied yet appear here beside what they owe, to be matched in one go.")}</StatePanel>
    );
  } else if (!company) {
    body = (
      <StatePanel kind="empty" title={tr("Choose a company")}>{tr("What is open is kept for one company at a time.")}</StatePanel>
    );
  } else if (q.error) body = <LoadFailed error={q.error} onRetry={() => void q.refetch()} title={tr("Could not load what is open")} />;
  else if (!q.data) body = <Loading label={tr("Loading what is open")} />;
  else body = <Body key={`${partyType}-${party}`} items={items} company={company} partyType={partyType} party={party} onApplied={refresh} />;

  return (
    <div className="erp-page erp-allocate">
      <PageHead
        title={tr("Apply payments")}
        meta={<Meta items={[tr("Apply payments, credit notes and journal credits to open invoices.")]} />}
      />
      <AccountsTabs />
      <div className="erp-bar">
        <div className="segmented" role="group" aria-label={tr("Whose")}>
          {(["Customer", "Supplier"] as const).map((k) => (
            <button key={k} type="button" aria-pressed={partyType === k} onClick={() => put({ type: k === "Customer" ? null : k, party: null, currency: null })}>
              {k === "Customer" ? tr("Customers") : tr("Suppliers")}
            </button>
          ))}
        </div>
        <Ctl label={partyType === "Customer" ? tr("Customer") : tr("Supplier")} htmlFor="al-party">
          <LinkInput to={partyType} id="al-party" placeholder={tr("Find a {partyType}…", { partyType: partyType.toLowerCase() })} value={party} onChange={(id) => put({ party: id, currency: null })} />
        </Ctl>
        <Ctl label={tr("Company")} htmlFor="al-company">
          <LinkInput to="Company" id="al-company" value={company} onChange={(id) => put({ company: id })} />
        </Ctl>
      </div>
      {body}
    </div>
  );
}
