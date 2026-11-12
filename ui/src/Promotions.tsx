// The promotions desk. A promotion's fields, grouped as a person thinks of them (what it is, when, on
// what conditions, what it gives, how often, how it stacks) change where they stand, as in a record's
// peek; its lists (customers, places, qualifying items) are the record's own form. Beside them, a test
// basket: a sample sale priced by the server's `price_lines` (price lists, pricing rules, promotions,
// taxes) over the promotions as saved, without saving anything or counting a code, and what the chosen
// promotion took from it. The second page is the Promotions Given report, by promotion.

import { useQuery } from "@tanstack/react-query";
import { useMemo, useRef, useState } from "react";
import { Link, useParams, useSearchParams } from "react-router";
import { api, errorMessage, isolate, t } from "@ferrum/client";
import { Alert, Badge, Button, Field, StatePanel, TabPanel, Tabs, useShellTitle } from "@ferrum/kit";
import { LinkInput, LinkTitle, RecordFields, useEntityMeta, useMe, useRecord, type EntityMeta } from "@ferrum/records";
import { Ctl, Meta, PageHead } from "./chrome";
import { act, amountInput, Gate, SetUp, useMissing, useRights } from "./desk";
import { DeskTabs } from "./DeskTabs";
import { Loading, LoadFailed } from "./states";
import { show } from "./money";
import { basketBody, type BasketLine, GROUPS, type Given, groupGiven, type Priced, rejectedCode, took } from "./promoRules";

const GROUP_WORD = (id: string) =>
  id === "name" ? t("Promotion") : id === "schedule" ? t("Schedule") : id === "conditions" ? t("Conditions") : id === "reward" ? t("What it gives") : id === "limits" ? t("Limits") : t("Stacking");

/** `/desk/promotions/:id?`: the promotion beside its test basket, and the Promotions Given report. */
export function Promotions() {
  useShellTitle(t("Promotions"));
  return (
    <Gate entity="Promotion">
      <Desk />
    </Gate>
  );
}

function Desk() {
  const { id } = useParams();
  const [sp, setSp] = useSearchParams();
  const tab = sp.get("tab") ?? "basket";
  return (
    <div className="erp-page">
      {id && <DeskTabs entity="Promotion" id={id} current="desk" />}
      <PageHead
        title={t("Promotions")}
        meta={<Meta items={[t("Try a sample sale against the promotions as saved, and see what each has given.")]} />}
        actions={
          <Link className="btn btn-ghost btn-md" to="/l/Promotion">
            {t("All promotions")}
          </Link>
        }
      />
      <Tabs
        id="promo"
        value={tab}
        onChange={(x) =>
          setSp((p) => {
            const n = new URLSearchParams(p);
            x === "basket" ? n.delete("tab") : n.set("tab", x);
            return n;
          }, { replace: true })
        }
        tabs={[
          { id: "basket", label: t("Promotion and test basket") },
          { id: "given", label: t("Promotions given") },
        ]}
      />
      <TabPanel id="promo" tab={tab}>
        {tab === "given" ? <GivenReport /> : <Editor id={id ?? null} />}
      </TabPanel>
    </div>
  );
}

function Editor({ id }: { id: string | null }) {
  const [priced, setPriced] = useState<Priced | null>(null);
  const none = useMissing("Promotion");
  if (none && !id) return <SetUp title={t("No promotion yet")} why={t("The test basket prices a sample sale against the promotions saved, and none is saved. Make one first.")} to="/f/Promotion/new" label={t("Make a promotion")} />;
  return (
    <div className="erp-two">
      <div className="erp-stack">{id ? <Fields id={id} /> : <StatePanel kind="empty" title={t("No promotion chosen")}>{t("Open a promotion from the list to see and change it here. The basket prices the company's promotions as saved.")}</StatePanel>}</div>
      <Basket promotion={id} priced={priced} onPriced={setPriced} />
    </div>
  );
}

function Fields({ id }: { id: string }) {
  const m = useEntityMeta("Promotion");
  const may = useRights("Promotion");
  if (m.error) return <LoadFailed error={m.error} onRetry={() => void m.refetch()} />;
  if (!m.data) return <Loading shape="form" label={t("Opening the promotion")} />;
  return <FieldGroups meta={m.data} id={id} canWrite={may.may("write")} />;
}

function FieldGroups({ meta, id, canWrite }: { meta: EntityMeta; id: string; canWrite: boolean }) {
  const { record, save } = useRecord(meta, id);
  // One meta per group, laid out as one section with the group's own heading.
  const groups = useMemo(
    () =>
      GROUPS.map((g) => ({
        id: g.id,
        meta: { ...meta, form: [{ label: GROUP_WORD(g.id), fields: g.fields }], fields: meta.fields.filter((f) => g.fields.includes(f.name)).map((f) => (canWrite ? f : { ...f, read_only: true })) } as EntityMeta,
      })),
    [meta, canWrite],
  );
  if (record.error) return <LoadFailed error={record.error} onRetry={() => void record.refetch()} title={t("Could not open the promotion")} />;
  if (!record.data) return <Loading shape="form" label={t("Opening the promotion")} />;
  return (
    <>
      {groups.map((g) => (
        <RecordFields key={g.id} meta={g.meta} doc={record.data} save={save} />
      ))}
      <p className="muted">
        {t("Its customers, places and qualifying items are kept in the record.")} <Link to={`/f/Promotion/${id}`}>{t("Open the record")}</Link>
      </p>
    </>
  );
}

const blankLine = (): BasketLine => ({ item: null, qty: "1", rate: "", discount: "" });

function Basket({ promotion, priced, onPriced }: { promotion: string | null; priced: Priced | null; onPriced: (p: Priced | null) => void }) {
  const me = useMe().data;
  const [company, setCompany] = useState<string | null>(null);
  const co = company ?? me?.default_company ?? null;
  const [customer, setCustomer] = useState<string | null>(null);
  const [channel, setChannel] = useState("");
  const [place, setPlace] = useState<string | null>(null);
  const [at, setAt] = useState("");
  const [codes, setCodes] = useState("");
  const [rows, setRows] = useState<BasketLine[]>([blankLine()]);
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  // The latest request wins: an answer that arrives after a newer press is dropped.
  const latest = useRef(0);
  const set = (i: number, c: Partial<BasketLine>) => setRows((x) => x.map((r, j) => (j === i ? { ...r, ...c } : r)));

  const price = async () => {
    if (!co || !customer || busy) return;
    const n = ++latest.current;
    setBusy(true);
    setProblem(null);
    try {
      const out = await act<Priced>("Company", co, "price_lines", basketBody({ customer, rows, channel, place, at, codes }));
      if (n === latest.current) onPriced(out);
    } catch (e) {
      if (n === latest.current) {
        onPriced(null);
        setProblem(errorMessage(e));
      }
    } finally {
      if (n === latest.current) setBusy(false);
    }
  };
  const mine = priced ? took(priced, promotion) : null;

  return (
    <section className="erp-panel" aria-labelledby="pb-title">
      <h2 id="pb-title" className="erp-card-title">{t("Test basket")}</h2>
      <p className="muted">{t("Nothing is saved and no code is counted.")}</p>
      <div className="erp-form-grid">
        <Field label={t("Company")} htmlFor="pb-company" required>
          <LinkInput to="Company" id="pb-company" value={co} onChange={setCompany} />
        </Field>
        <Field label={t("Customer")} htmlFor="pb-customer" required>
          <LinkInput to="Customer" id="pb-customer" value={customer} onChange={setCustomer} />
        </Field>
        <Field label={t("Channel")} htmlFor="pb-channel" hint={t("Where the sale is made, as documents name it.")}>
          <input id="pb-channel" className="input" value={channel} onChange={(e) => setChannel(e.target.value)} />
        </Field>
        <Field label={t("Place")} htmlFor="pb-place">
          <LinkInput to="Warehouse" id="pb-place" value={place} onChange={setPlace} />
        </Field>
        <Field label={t("At")} htmlFor="pb-at" hint={t("Empty: now, on the company's clock.")}>
          <input id="pb-at" className="input" type="datetime-local" value={at} onChange={(e) => setAt(e.target.value)} />
        </Field>
        <Field label={t("Codes")} htmlFor="pb-codes" hint={t("Separated by commas or spaces.")}>
          <input id="pb-codes" className="input" value={codes} autoComplete="off" onChange={(e) => setCodes(e.target.value)} />
        </Field>
      </div>
      <div className="table-wrap">
        <table className="table" aria-label={t("Sample lines")}>
          <thead>
            <tr>
              <th scope="col">{t("Item")}</th>
              <th scope="col" className="num">{t("Quantity")}</th>
              <th scope="col" className="num">{t("Rate")}</th>
              <th scope="col" className="num">{t("Discount %")}</th>
              <th scope="col"><span className="sr-only">{t("Remove")}</span></th>
            </tr>
          </thead>
          <tbody>
            {rows.map((r, i) => (
              <tr key={i}>
                <td>
                  <LinkInput to="Item" id={`pb-item-${i}`} value={r.item} onChange={(v) => set(i, { item: v })} />
                </td>
                <td className="num">{amountInput(r.qty, (v) => set(i, { qty: v }), { label: t("Quantity of line {n}", { n: i + 1 }) })}</td>
                <td className="num">{amountInput(r.rate, (v) => set(i, { rate: v }), { label: t("Rate of line {n}", { n: i + 1 }) })}</td>
                <td className="num">{amountInput(r.discount, (v) => set(i, { discount: v }), { label: t("Discount of line {n}", { n: i + 1 }) })}</td>
                <td>
                  <Button variant="ghost" aria-label={t("Remove line {n}", { n: i + 1 })} disabled={rows.length === 1} onClick={() => setRows((x) => x.filter((_, j) => j !== i))}>
                    {t("Remove")}
                  </Button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <div className="erp-actions">
        <Button onClick={() => setRows((x) => [...x, blankLine()])}>{t("Add a line")}</Button>
        <Button variant="primary" loading={busy} disabled={!co || !customer || rows.every((r) => !r.item)} onClick={() => void price()}>
          {t("Price the basket")}
        </Button>
      </div>
      {problem && <Alert tone="error">{isolate(problem)}</Alert>}
      {priced && <Result priced={priced} promotion={promotion} mine={mine} />}
    </section>
  );
}

function Result({ priced, promotion, mine }: { priced: Priced; promotion: string | null; mine: ReturnType<typeof took> }) {
  const cur = priced.currency ? isolate(priced.currency) : "";
  return (
    <div className="erp-stack" aria-live="polite">
      {promotion && (mine ? <Alert tone="success">{t("This promotion took {amount} off this basket.", { amount: show(mine.amount) })}</Alert> : <Alert tone="info">{t("This promotion did not apply to this basket.")}</Alert>)}
      {priced.rejected_codes.map((c) => {
        const r = rejectedCode(c);
        return (
          <Alert key={r.code} tone="warning">
            {t("The code {code} cannot be used.", { code: r.code })} {r.reason ? isolate(r.reason) : ""}
          </Alert>
        );
      })}
      <div className="table-wrap">
        <table className="table" aria-label={t("Priced lines")}>
          <thead>
            <tr>
              <th scope="col">{t("Item")}</th>
              <th scope="col" className="num">{t("Quantity")}</th>
              <th scope="col" className="num">{t("List rate")}</th>
              <th scope="col" className="num">{t("Rate")}</th>
              <th scope="col" className="num">{t("Amount")}</th>
            </tr>
          </thead>
          <tbody>
            {priced.lines.map((l, i) => (
              <tr key={i}>
                <td>
                  <LinkTitle to="Item" id={l.item} /> {l.added_by_promotion && <Badge tone="purple">{t("Added by a promotion")}</Badge>}
                  {mine?.lines.includes(i) && <Badge tone="green">{t("Discounted by this promotion")}</Badge>}
                </td>
                <td className="num">{isolate(show(l.qty))}</td>
                <td className="num">{isolate(show(l.price_list_rate))}</td>
                <td className="num">{isolate(show(l.rate))}</td>
                <td className="num">{isolate(show(l.amount))}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {priced.promotions.length === 0 ? (
        <p className="muted">{t("No promotion applied.")}</p>
      ) : (
        <ul className="erp-list" aria-label={t("Promotions applied")}>
          {priced.promotions.map((p, i) => (
            <li key={i}>
              {p.promotion ? <Link to={`/desk/promotions/${p.promotion}`}>{isolate(p.title ?? "")}</Link> : isolate(p.title ?? "")} · {isolate(show(p.amount))}
              {p.code && <> · {isolate(p.code)}</>}
              {p.line !== null && <> · {t("line {n}", { n: p.line + 1 })}</>}
            </li>
          ))}
        </ul>
      )}
      <dl className="erp-figures">
        <div className="erp-figure-row erp-four">
          <div className="erp-figure">
            <dt>{t("Total")}</dt>
            <dd>{isolate(show(priced.total))}</dd>
            {cur && <dd className="faint">{cur}</dd>}
          </div>
          <div className="erp-figure">
            <dt>{t("Net total")}</dt>
            <dd>{isolate(show(priced.net_total))}</dd>
          </div>
          <div className="erp-figure">
            <dt>{t("Taxes")}</dt>
            <dd>{isolate(show(priced.total_taxes))}</dd>
          </div>
          <div className="erp-figure">
            <dt>{t("Grand total")}</dt>
            <dd>{isolate(show(priced.rounded_total ?? priced.grand_total))}</dd>
          </div>
        </div>
      </dl>
    </div>
  );
}

interface GivenReportData {
  rows: Given[];
  totals?: { discount?: string } | null;
  meta?: { from_date?: string; to_date?: string } | null;
}

function GivenReport() {
  const me = useMe().data;
  const [company, setCompany] = useState<string | null>(null);
  const co = company ?? me?.default_company ?? null;
  const [from, setFrom] = useState("");
  const [to, setTo] = useState("");
  const q = useQuery({
    queryKey: ["erp", "promotions-given", co, from, to],
    enabled: !!co,
    queryFn: ({ signal }) => api.get<GivenReportData>("/api/reports/promotions", { company: co ?? "", ...(from ? { from_date: from } : {}), ...(to ? { to_date: to } : {}) }, signal),
  });
  const groups = useMemo(() => groupGiven(q.data?.rows ?? []), [q.data]);
  return (
    <div className="erp-stack">
      <form className="erp-bar" onSubmit={(e) => e.preventDefault()} aria-label={t("Which sales")}>
        <Ctl label={t("Company")} htmlFor="pg-company">
          <LinkInput to="Company" id="pg-company" value={co} onChange={setCompany} />
        </Ctl>
        <Ctl label={t("From")} htmlFor="pg-from">
          <input id="pg-from" className="input" type="date" value={from} onChange={(e) => setFrom(e.target.value)} />
        </Ctl>
        <Ctl label={t("To")} htmlFor="pg-to">
          <input id="pg-to" className="input" type="date" value={to} onChange={(e) => setTo(e.target.value)} />
        </Ctl>
      </form>
      {!co ? (
        <StatePanel kind="empty" title={t("Choose a company")} />
      ) : q.error ? (
        <LoadFailed error={q.error} onRetry={() => void q.refetch()} title={t("Could not load the report")} />
      ) : !q.data ? (
        <Loading columns={4} label={t("Loading the report")} />
      ) : groups.length === 0 ? (
        <StatePanel kind="empty" title={t("No promotion was given in this period")} />
      ) : (
        <div className="table-wrap">
          <table className="table" aria-label={t("Promotions given")}>
            <thead>
              <tr>
                <th scope="col">{t("Promotion")}</th>
                <th scope="col">{t("Month")}</th>
                <th scope="col">{t("Channel")}</th>
                <th scope="col" className="num">{t("Documents")}</th>
                <th scope="col" className="num">{t("Discount given")}</th>
              </tr>
            </thead>
            {groups.map((g) => (
              <tbody key={g.key}>
                <tr className="erp-group-row">
                  <th scope="rowgroup" colSpan={4}>
                    {g.promotion_id ? <Link to={`/desk/promotions/${g.promotion_id}`}>{isolate(g.promotion)}</Link> : isolate(g.promotion)}
                  </th>
                  <td className="num">{isolate(show(g.discount))}</td>
                </tr>
                {g.rows.map((r, i) => (
                  <tr key={i}>
                    <td />
                    <td>{isolate(r.period)}</td>
                    <td>{isolate(r.channel)}</td>
                    <td className="num">{r.documents}</td>
                    <td className="num">{isolate(show(r.discount))}</td>
                  </tr>
                ))}
              </tbody>
            ))}
            <tfoot>
              <tr>
                <td colSpan={4}>{t("Total")}</td>
                <td className="num">{isolate(show(q.data.totals?.discount))}</td>
              </tr>
            </tfoot>
          </table>
        </div>
      )}
    </div>
  );
}
