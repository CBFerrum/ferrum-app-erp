// The planning desk. The MRP run's buckets per item and warehouse, with the suggested orders to
// accept (in bulk: what is left out is set to nothing, and the server makes purchase orders, work
// orders and the transfer from the rest); the promise-date check of an order's lines, from what is
// available and coming in; and the stock held for an order's lines under the company's reservation
// policy. The periods, quantities and dates are the server's; its refusals are shown as they are.

import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Link, useSearchParams } from "react-router";
import { api, docPath, errorMessage, formatDate, isolate, newKey, t } from "@ferrum/client";
import { Alert, Badge, Button, confirmDialog, StatePanel, TabPanel, Tabs, toast, useShellTitle } from "@ferrum/kit";
import { LinkInput, LinkTitle, useMe } from "@ferrum/records";
import { Ctl, Meta, PageHead } from "./chrome";
import { act, Gate, useRights } from "./desk";
import { Loading, LoadFailed } from "./states";
import { show } from "./money";
import { acceptBody, type Availability, availabilityBody, buckets, defaults, held, keeps, type MrpRun, needSupplier, open, type Order, type OrderLine, reservable, type Supplied, tally } from "./plans";

const ACTION = (a: string) => (a === "Purchase" ? t("Buy") : a === "Manufacture" ? t("Make") : a === "Transfer" ? t("Transfer") : isolate(a));
const MESSAGE = (m: string) => (m === "Bring Forward" ? t("Bring forward") : m === "Push Out" ? t("Push out") : m === "Cancel" ? t("Cancel") : m === "Increase" ? t("Increase") : m === "Decrease" ? t("Decrease") : isolate(m));

/** `/desk/planning`. */
export function Planning() {
  useShellTitle(t("Planning"));
  return (
    <Gate entity="MrpRun">
      <Desk />
    </Gate>
  );
}

function Desk() {
  const [sp, setSp] = useSearchParams();
  const tab = sp.get("tab") ?? "mrp";
  const put = (k: string, v: string | null) => setSp((p) => { const n = new URLSearchParams(p); v ? n.set(k, v) : n.delete(k); return n; }, { replace: true });
  return (
    <div className="erp-page">
      <PageHead title={t("Planning")} meta={<Meta items={[t("What to buy, make and move; what can be promised; what is held for orders.")]} />} />
      <Tabs
        id="plan"
        value={tab}
        onChange={(x) => put("tab", x === "mrp" ? null : x)}
        tabs={[
          { id: "mrp", label: t("Planned orders") },
          { id: "promise", label: t("Promise dates") },
          { id: "reserve", label: t("Reservations") },
        ]}
      />
      <TabPanel id="plan" tab={tab}>
        {tab === "promise" ? <OrderTab mode="promise" order={sp.get("order")} onOrder={(id) => put("order", id)} /> : tab === "reserve" ? <OrderTab mode="reserve" order={sp.get("order")} onOrder={(id) => put("order", id)} /> : <Mrp run={sp.get("run")} onRun={(id) => put("run", id)} />}
      </TabPanel>
    </div>
  );
}

function Mrp({ run, onRun }: { run: string | null; onRun: (id: string | null) => void }) {
  const qc = useQueryClient();
  const may = useRights("MrpRun").may;
  const q = useQuery({ queryKey: ["erp", "mrp", run], enabled: !!run, queryFn: ({ signal }) => api.get<MrpRun>(docPath("MrpRun", run ?? ""), undefined, signal) });
  const [taken, setTaken] = useState<Set<string> | null>(null);
  const [sup, setSup] = useState<Supplied>({});
  const [busy, setBusy] = useState<null | "run" | "accept">(null);
  const [problem, setProblem] = useState<string | null>(null);
  const [made, setMade] = useState<{ entity: string; id: string; number?: string | null }[]>([]);
  const r = q.data;
  const keep = (x: MrpRun) => {
    qc.setQueryData(["erp", "mrp", run], x);
    setTaken(null);
    setSup({});
  };
  const go = async (what: "run" | "accept", f: () => Promise<void>) => {
    if (busy) return;
    setBusy(what);
    setProblem(null);
    try {
      await f();
    } catch (e) {
      setProblem(errorMessage(e));
    } finally {
      setBusy(null);
    }
  };

  const picker = (
    <form className="erp-bar" onSubmit={(e) => e.preventDefault()} aria-label={t("Which run")}>
      <Ctl label={t("MRP run")} htmlFor="mrp-run">
        <LinkInput to="MrpRun" id="mrp-run" value={run} onChange={onRun} />
      </Ctl>
      <Link className="btn btn-ghost btn-md" to="/e/MrpRun/new">{t("New run")}</Link>
    </form>
  );
  if (!run) return <div className="erp-stack">{picker}<StatePanel kind="empty" title={t("Choose a run")}>{t("A run plans purchases, production and transfers from demand, stock and what is on order.")}</StatePanel></div>;
  if (q.error) return <div className="erp-stack">{picker}<LoadFailed error={q.error} onRetry={() => void q.refetch()} title={t("Could not open the run")} /></div>;
  if (!r) return <div className="erp-stack">{picker}<Loading label={t("Opening the run")} /></div>;

  const on = taken ?? defaults(r.lines);
  const { periods, rows } = buckets(r.lines);
  const missing = needSupplier(r.lines, on, sup);
  const counts = tally(r.lines, on);
  const draft = r.docstatus === 0;
  const toggle = (id: string) => setTaken((x) => { const n = new Set(x ?? on); n.has(id) ? n.delete(id) : n.add(id); return n; });

  const runIt = () => go("run", async () => { keep(await act<MrpRun>("MrpRun", run, "run")); toast(t("Planned")); });
  const accept = async () => {
    const ok = await confirmDialog({
      title: t("Make these orders?"),
      message: t("{buy} purchase lines, {make} to make, {move} to transfer. Lines left out are set to nothing; run the plan again to get them back.", { buy: counts.Purchase, make: counts.Manufacture, move: counts.Transfer }),
      confirmLabel: t("Make the orders"),
    });
    if (!ok) return;
    await go("accept", async () => {
      const saved = await api.patch<MrpRun>(docPath("MrpRun", run), acceptBody(r.lines, on, sup), { ifMatch: r.version });
      const out = await act<{ created?: { entity: string; id: string; number?: string | null }[] }>("MrpRun", run, "create_orders", {}, newKey());
      setMade(out.created ?? []);
      keep(await api.get<MrpRun>(docPath("MrpRun", run)).catch(() => saved));
      toast(t("Orders made"));
    });
  };

  return (
    <div className="erp-stack">
      {picker}
      {problem && <Alert tone="error">{isolate(problem)}</Alert>}
      {made.length > 0 && (
        <Alert tone="success">
          {t("Made:")}{" "}
          {made.map((m, i) => (
            <span key={m.id}>{i > 0 && ", "}<Link to={`/e/${m.entity}/${m.id}`}>{isolate(m.number ?? m.id)}</Link></span>
          ))}
        </Alert>
      )}
      {r.lines.length === 0 ? (
        <StatePanel kind="empty" title={t("Nothing is planned")} action={draft && may("write") ? <Button variant="primary" loading={busy === "run"} onClick={() => void runIt()}>{t("Run the plan")}</Button> : undefined}>
          {t("Run the plan to see what to buy, make and move.")}
        </StatePanel>
      ) : (
        <div className="table-wrap">
          <table className="table" aria-label={t("Planned orders by period")}>
            <thead>
              <tr>
                <th scope="col"><span className="sr-only">{t("Take")}</span></th>
                <th scope="col">{t("Item")}</th>
                <th scope="col">{t("Warehouse")}</th>
                {periods.map((p) => <th key={p} scope="col" className="num">{p ? formatDate(p) : t("Total")}</th>)}
              </tr>
            </thead>
            <tbody>
              {rows.map((row) => (
                <tr key={row.key}>
                  <td />
                  <th scope="row">{row.item ? <LinkTitle to="Item" id={row.item} /> : isolate(row.item_name)}</th>
                  <td>{row.warehouse ? <LinkTitle to="Warehouse" id={row.warehouse} /> : ""}</td>
                  {periods.map((p) => (
                    <td key={p} className="num">
                      {(row.cells.get(p) ?? []).map((l, i) => (
                        <div key={l.id ?? i} className="erp-stack">
                          <label className="check-field">
                            {l.id && open(l) && draft && may("write") && (
                              <input type="checkbox" aria-label={t("Take {item}, {action}", { item: l.item_name ?? l.item, action: ACTION(l.action) })} checked={on.has(l.id)} onChange={() => toggle(l.id as string)} />
                            )}
                            <span>
                              {ACTION(l.action)} {isolate(show(l.suggested_qty))}
                              {l.created_document && <> <Badge tone="green">{isolate(l.created_document)}</Badge></>}
                            </span>
                          </label>
                          <span className="muted">
                            {t("needs {net}, has {have}, on order {order}", { net: show(l.net_requirement), have: show(l.available), order: show(l.on_order) })}
                          </span>
                          {l.action === "Purchase" && l.id && open(l) && draft && (
                            <LinkInput to="Supplier" id={`mrp-sup-${l.id}`} value={l.id in sup ? sup[l.id] : (l.supplier ?? null)} placeholder={t("Supplier")} onChange={(v) => setSup((s) => ({ ...s, [l.id as string]: v }))} />
                          )}
                        </div>
                      ))}
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {r.messages.length > 0 && (
        <section className="erp-panel" aria-labelledby="mrp-msgs">
          <h2 id="mrp-msgs" className="erp-card-title">{t("Orders already placed that should change")}</h2>
          <ul className="erp-list">
            {r.messages.map((m, i) => (
              <li key={i}>
                <Badge tone="amber">{MESSAGE(m.message)}</Badge> {isolate(`${m.document_type ?? ""} ${m.document ?? ""}`.trim())}
                {m.suggested_date && <> · {t("from {from} to {to}", { from: m.current_date ? formatDate(m.current_date) : "", to: formatDate(m.suggested_date) })}</>}
                {m.suggested_qty && <> · {t("quantity {from} to {to}", { from: show(m.current_qty), to: show(m.suggested_qty) })}</>}
              </li>
            ))}
          </ul>
        </section>
      )}
      <div className="erp-sticky-foot">
        <span className="muted" aria-live="polite">
          {missing.length > 0 ? t("{count} purchase lines need a supplier.", { count: missing.length }) : !may("write") ? t("You can look at the plan. A planner makes the orders.") : r.status === "Orders Created" ? t("Orders have been made from this run.") : ""}
        </span>
        <div className="erp-actions">
          {draft && <Button disabled={!may("write") || busy !== null} loading={busy === "run"} onClick={() => void runIt()}>{t("Run the plan again")}</Button>}
          <Button variant="primary" disabled={!may("write") || !draft || r.status !== "Planned" || on.size === 0 || missing.length > 0 || busy !== null} loading={busy === "accept"} onClick={() => void accept()}>
            {t("Make the orders taken")}
          </Button>
        </div>
      </div>
    </div>
  );
}

function OrderTab({ mode, order, onOrder }: { mode: "promise" | "reserve"; order: string | null; onOrder: (id: string | null) => void }) {
  const q = useQuery({ queryKey: ["erp", "plan-order", order], enabled: !!order, queryFn: ({ signal }) => api.get<Order>(docPath("SalesOrder", order ?? ""), undefined, signal) });
  const picker = (
    <form className="erp-bar" onSubmit={(e) => e.preventDefault()} aria-label={t("Which order")}>
      <Ctl label={t("Sales order")} htmlFor="plan-order">
        <LinkInput to="SalesOrder" id="plan-order" value={order} onChange={onOrder} />
      </Ctl>
    </form>
  );
  const body = !order ? (
    <StatePanel kind="empty" title={t("Choose an order")} />
  ) : q.error ? (
    <LoadFailed error={q.error} onRetry={() => void q.refetch()} title={t("Could not open the order")} />
  ) : !q.data ? (
    <Loading label={t("Opening the order")} />
  ) : mode === "promise" ? (
    <PromiseTab order={q.data} />
  ) : (
    <Reserve order={q.data} />
  );
  return <div className="erp-stack">{picker}{body}</div>;
}

function PromiseTab({ order }: { order: Order }) {
  const me = useMe().data;
  const [result, setResult] = useState<{ line: number; a: Availability } | null>(null);
  const [busy, setBusy] = useState<number | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const check = async (l: OrderLine, i: number) => {
    if (busy !== null) return;
    setBusy(i);
    setProblem(null);
    try {
      const a = await act<Availability>("Company", order.company || me?.default_company || "", "availability", availabilityBody({ item: l.item, qty: l.qty, warehouse: l.warehouse, date: null }));
      setResult({ line: i, a });
    } catch (e) {
      setProblem(errorMessage(e));
    } finally {
      setBusy(null);
    }
  };
  return (
    <div className="erp-stack">
      {problem && <Alert tone="error">{isolate(problem)}</Alert>}
      <div className="table-wrap">
        <table className="table" aria-label={t("Lines and their promised dates")}>
          <thead>
            <tr>
              <th scope="col">{t("Item")}</th>
              <th scope="col" className="num">{t("Quantity")}</th>
              <th scope="col">{t("Warehouse")}</th>
              <th scope="col" className="num">{t("Available when entered")}</th>
              <th scope="col">{t("Wanted by")}</th>
              <th scope="col">{t("Promised")}</th>
              <th scope="col"><span className="sr-only">{t("Check")}</span></th>
            </tr>
          </thead>
          <tbody>
            {order.items.map((l, i) => {
              const ok = keeps(l);
              return (
                <tr key={l.id ?? i}>
                  <td><LinkTitle to="Item" id={l.item} /></td>
                  <td className="num">{isolate(show(l.qty))}</td>
                  <td>{l.warehouse ? <LinkTitle to="Warehouse" id={l.warehouse} /> : ""}</td>
                  <td className="num">{isolate(show(l.available_qty))}</td>
                  <td>{l.delivery_date ? formatDate(l.delivery_date) : ""}</td>
                  <td>
                    {l.promised_date ? formatDate(l.promised_date) : ""} {ok === false && <Badge tone="red">{t("Later than wanted")}</Badge>}
                  </td>
                  <td>
                    <Button loading={busy === i} disabled={busy !== null} onClick={() => void check(l, i)}>
                      {t("Check now")}
                    </Button>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      {result && <AvailabilityView a={result.a} />}
    </div>
  );
}

function AvailabilityView({ a }: { a: Availability }) {
  return (
    <section className="erp-panel" aria-labelledby="plan-avail" aria-live="polite">
      <h2 id="plan-avail" className="erp-card-title">{t("What can be promised")}</h2>
      <p>{a.promise_date ? t("The quantity can be promised for {date}.", { date: formatDate(a.promise_date) }) : t("No date can be promised from stock and orders: the item's lead time of {days} days applies.", { days: a.lead_time_days ?? 0 })}</p>
      <div className="table-wrap">
        <table className="table">
          <thead>
            <tr>
              <th scope="col">{t("Warehouse")}</th>
              <th scope="col" className="num">{t("On hand")}</th>
              <th scope="col" className="num">{t("Reserved")}</th>
              <th scope="col" className="num">{t("Held")}</th>
              <th scope="col" className="num">{t("Available now")}</th>
              <th scope="col">{t("Coming in")}</th>
              <th scope="col">{t("Promise")}</th>
            </tr>
          </thead>
          <tbody>
            {a.warehouses.map((w) => (
              <tr key={w.warehouse}>
                <td><LinkTitle to="Warehouse" id={w.warehouse} /></td>
                <td className="num">{isolate(show(w.on_hand))}</td>
                <td className="num">{isolate(show(w.reserved))}</td>
                <td className="num">{isolate(show(w.held))}</td>
                <td className="num">{isolate(show(w.available_now))}</td>
                <td>{w.incoming.map((x) => `${formatDate(x.date)}: ${show(x.qty)}`).join(", ")}</td>
                <td>{w.promise_date ? formatDate(w.promise_date) : ""}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}

function Reserve({ order }: { order: Order }) {
  const qc = useQueryClient();
  const may = useRights("SalesOrder").may;
  const co = useQuery({ queryKey: ["erp", "plan-company", order.company], queryFn: ({ signal }) => api.get<{ reservation_policy?: "Count Only" | "Hold"; reserve_on?: string | null }>(docPath("Company", order.company), undefined, signal) });
  const [busy, setBusy] = useState<null | "reserve_stock" | "release_stock">(null);
  const [problem, setProblem] = useState<string | null>(null);
  const run = async (name: "reserve_stock" | "release_stock") => {
    if (busy) return;
    if (name === "release_stock" && !(await confirmDialog({ title: t("Release the reservation?"), message: t("The stock held for this order is free again for others."), confirmLabel: t("Release") }))) return;
    setBusy(name);
    setProblem(null);
    try {
      await act("SalesOrder", order.id, name, {}, newKey());
      qc.setQueryData(["erp", "plan-order", order.id], await api.get<Order>(docPath("SalesOrder", order.id)));
      toast(name === "reserve_stock" ? t("Stock reserved") : t("Reservation released"));
    } catch (e) {
      setProblem(errorMessage(e));
    } finally {
      setBusy(null);
    }
  };
  const holding = co.data?.reservation_policy === "Hold";
  return (
    <div className="erp-stack">
      {co.data && (holding ? <Alert tone="info">{t("This company holds stock for orders: what is free is held when the order is submitted or changed, and the rest is back-ordered.")}</Alert> : <Alert tone="info">{t("This company only counts open orders against what can be promised: no stock is held, and anyone may take it.")}</Alert>)}
      {problem && <Alert tone="error">{isolate(problem)}</Alert>}
      <div className="table-wrap">
        <table className="table" aria-label={t("Reservations by line")}>
          <thead>
            <tr>
              <th scope="col">{t("Item")}</th>
              <th scope="col" className="num">{t("Quantity")}</th>
              <th scope="col" className="num">{t("Held")}</th>
              <th scope="col" className="num">{t("Back-ordered")}</th>
              <th scope="col">{t("Held until")}</th>
            </tr>
          </thead>
          <tbody>
            {order.items.map((l, i) => {
              const h = held(l);
              return (
                <tr key={l.id ?? i}>
                  <td><LinkTitle to="Item" id={l.item} /></td>
                  <td className="num">{isolate(show(l.qty))}</td>
                  <td className="num">{isolate(show(h.reserved))}</td>
                  <td className="num">{isolate(show(h.short))}</td>
                  <td>{l.reserved_until ? formatDate(l.reserved_until) : ""}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      <div className="erp-actions">
        <Button disabled={!may("submit") || !reservable(order) || !holding || busy !== null} loading={busy === "reserve_stock"} onClick={() => void run("reserve_stock")}>
          {t("Reserve stock")}
        </Button>
        <Button disabled={!may("submit") || !reservable(order) || !holding || busy !== null} loading={busy === "release_stock"} onClick={() => void run("release_stock")}>
          {t("Release the reservation")}
        </Button>
      </div>
      {!reservable(order) && <p className="muted">{t("A reservation is made on a submitted order that is not closed or fully delivered.")}</p>}
    </div>
  );
}
