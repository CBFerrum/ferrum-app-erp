// The stored value desk (src/stored_value.rs): find a card by its code and see its balance, what is
// held for orders and its movements; issue a new card; top one up; hold value for an order and release
// the hold. A card's code and PIN are asked in password boxes, are sent only to the kind's `prove` or
// `balance` as the server wants them, and are cleared as the call ends, success or refusal. They are
// never put in the address, a query's key, a toast, the console or storage, and a code the server
// makes for a new card is shown once in a dialog and then gone. Calls are made directly (no cached
// query keeps their arguments).

import { useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { useSearchParams } from "react-router";
import { api, docPath, errorMessage, formatDate, formatDateTime, isolate, newKey, t } from "@ferrum/client";
import { Alert, Badge, Button, confirmDialog, Field, StatePanel, TabPanel, Tabs, toast, useShellTitle } from "@ferrum/kit";
import { LinkInput, LinkTitle } from "@ferrum/records";
import { Ctl, Meta, PageHead } from "./chrome";
import { act, amountInput, Gate, useRights } from "./desk";
import { Loading, LoadFailed } from "./states";
import { show } from "./money";
import { balanceBody, type Balance, carriesSecret, entryWord, type Hold, holdBody, issueBody, type Kind, type Proof, proveBody, running, topUpBody } from "./storedValues";

/** The page `/desk/stored-value` (and a new account: `/e/StoredValueAccount/new`). */
export function StoredValue() {
  useShellTitle(t("Stored value"));
  return (
    <Gate entity="StoredValueKind">
      <Desk />
    </Gate>
  );
}

const STATE_TONE: Record<string, "green" | "amber" | "red" | "neutral"> = { Active: "green", Blocked: "red", Expired: "amber", "Used Up": "neutral" };
const stateWord = (s: string) => (s === "Active" ? t("Active") : s === "Blocked" ? t("Blocked") : s === "Expired" ? t("Expired") : s === "Used Up" ? t("Used up") : isolate(s));

/** A box for a card's code or PIN: a password box that is never filled in by the browser and is cleared by its owner. */
function Secret({ id, label, value, onChange, hint }: { id: string; label: string; value: string; onChange: (v: string) => void; hint?: string }) {
  return (
    <Field label={label} htmlFor={id} hint={hint}>
      <input id={id} className="input" type="password" autoComplete="new-password" autoCapitalize="off" spellCheck={false} data-1p-ignore data-lpignore="true" value={value} onChange={(e) => onChange(e.target.value)} />
    </Field>
  );
}

function Desk() {
  const [sp, setSp] = useSearchParams();
  const kindId = sp.get("kind");
  const tab = sp.get("tab") ?? "find";
  const put = (k: string, v: string | null) =>
    setSp((p) => {
      const n = new URLSearchParams(p);
      v ? n.set(k, v) : n.delete(k);
      return n;
    }, { replace: true });
  const kind = useQuery({ queryKey: ["erp", "sv-kind", kindId], enabled: !!kindId, queryFn: ({ signal }) => api.get<Kind>(docPath("StoredValueKind", kindId ?? ""), undefined, signal) });
  return (
    <div className="erp-page">
      <PageHead title={t("Stored value")} meta={<Meta items={[t("Gift cards, store credit and prepaid packages: find one by its code, issue one, top one up, hold value for an order.")]} />} />
      <form className="erp-bar" onSubmit={(e) => e.preventDefault()} aria-label={t("Which kind")}>
        <Ctl label={t("Kind")} htmlFor="sv-kind">
          <LinkInput to="StoredValueKind" id="sv-kind" value={kindId} onChange={(id) => put("kind", id)} />
        </Ctl>
      </form>
      {!kindId ? (
        <StatePanel kind="empty" title={t("Choose a kind")}>{t("A card belongs to one kind of stored value; its rules say what can be done with it.")}</StatePanel>
      ) : kind.error ? (
        <LoadFailed error={kind.error} onRetry={() => void kind.refetch()} title={t("Could not open the kind")} />
      ) : !kind.data ? (
        <Loading shape="form" label={t("Opening the kind")} />
      ) : (
        <>
          <Tabs
            id="sv"
            value={tab}
            onChange={(x) => put("tab", x === "find" ? null : x)}
            tabs={[
              { id: "find", label: t("Find a card") },
              { id: "issue", label: t("Issue a new card") },
            ]}
          />
          <TabPanel id="sv" tab={tab}>
            {tab === "issue" ? <Issue kind={kind.data} /> : <Find kind={kind.data} />}
          </TabPanel>
        </>
      )}
    </div>
  );
}

function Find({ kind }: { kind: Kind }) {
  const account = useRights("StoredValueAccount");
  const [code, setCode] = useState("");
  const [pin, setPin] = useState("");
  const [found, setFound] = useState<Balance | null>(null);
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const [holds, setHolds] = useState<Hold[]>([]);
  const units = kind.holds === "Units" ? (kind.unit_name ?? t("units")) : "";

  const look = async () => {
    if (busy) return;
    setBusy(true);
    setProblem(null);
    const body = balanceBody(code, pin);
    // The boxes are emptied before the answer is waited for: nothing of the code stays on the page.
    setCode("");
    setPin("");
    try {
      setFound(await act<Balance>("StoredValueKind", kind.id, "balance", body));
    } catch (e) {
      setFound(null);
      setProblem(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="erp-stack">
      <form
        className="erp-form-grid"
        autoComplete="off"
        onSubmit={(e) => {
          e.preventDefault();
          void look();
        }}
      >
        <Secret id="sv-code" label={t("Code")} value={code} onChange={setCode} />
        {kind.requires_pin && <Secret id="sv-pin" label={t("PIN")} value={pin} onChange={setPin} />}
        <div className="erp-actions">
          <Button type="submit" variant="primary" loading={busy} disabled={!code.trim() || (kind.requires_pin && !pin.trim())}>
            {t("Look up")}
          </Button>
        </div>
      </form>
      {problem && <Alert tone="error">{isolate(problem)}</Alert>}
      {found && (
        <>
          <dl className="erp-figures">
            <div className="erp-figure-row erp-four">
              <div className="erp-figure">
                <dt>{t("Balance")}</dt>
                <dd>{isolate(show(found.balance))}</dd>
                {units && <dd className="faint">{isolate(units)}</dd>}
              </div>
              <div className="erp-figure">
                <dt>{t("Held for orders")}</dt>
                <dd>{isolate(show(found.held))}</dd>
              </div>
              <div className="erp-figure">
                <dt>{t("Available")}</dt>
                <dd>{isolate(show(found.available))}</dd>
              </div>
              <div className="erp-figure">
                <dt>{t("State")}</dt>
                <dd>
                  <Badge tone={STATE_TONE[found.state] ?? "neutral"}>{stateWord(found.state)}</Badge>
                </dd>
                {found.expires_on && <dd className="faint">{t("Expires {date}", { date: formatDate(found.expires_on) })}</dd>}
              </div>
            </div>
          </dl>
          {account.may("read") && (
            <p>
              <LinkTitle to="StoredValueAccount" id={found.account} />
            </p>
          )}
          <Movements kind={kind} account={found.account} />
          {kind.can_top_up && account.may("write") && <TopUp account={found.account} onDone={() => setFound(null)} />}
          <HoldValue kind={kind} onHeld={(h) => setHolds((x) => [h, ...x])} />
          <Holds kind={kind} holds={holds} onReleased={(h) => setHolds((x) => x.filter((y) => y.hold !== h))} />
        </>
      )}
    </div>
  );
}

interface Statement {
  rows: { posting_date?: string; entry_type: string; amount?: string; balance: string; remarks?: string | null }[];
  meta?: { truncated?: boolean } | null;
}

function Movements({ kind, account }: { kind: Kind; account: string }) {
  const q = useQuery({
    queryKey: ["erp", "sv-statement", kind.company, account],
    queryFn: ({ signal }) => api.get<Statement>("/api/reports/stored_value_statement", { company: kind.company, stored_value_account: account }, signal),
  });
  const words: Record<string, string> = {
    Opening: t("Opening"),
    issue: t("Issued"),
    redeem: t("Used"),
    refund: t("Refunded onto it"),
    transfer: t("Transferred"),
    adjust: t("Adjusted"),
    expire: t("Expired"),
    breakage: t("Unused value recognised"),
    reverse: t("Reversed"),
  };
  return (
    <section className="erp-panel" aria-labelledby="sv-moves">
      <h2 id="sv-moves" className="erp-card-title">{t("Movements")}</h2>
      {q.error ? (
        <LoadFailed error={q.error} onRetry={() => void q.refetch()} title={t("Could not load the movements")} />
      ) : !q.data ? (
        <Loading columns={4} rows={3} label={t("Loading the movements")} />
      ) : q.data.rows.length <= 1 ? (
        <p className="muted">{t("Nothing has moved on this card in the period.")}</p>
      ) : (
        <div className="table-wrap">
          <table className="table">
            <thead>
              <tr>
                <th scope="col">{t("Date")}</th>
                <th scope="col">{t("Entry")}</th>
                <th scope="col" className="num">{t("Amount")}</th>
                <th scope="col" className="num">{t("Balance")}</th>
                <th scope="col">{t("Remarks")}</th>
              </tr>
            </thead>
            <tbody>
              {q.data.rows.map((r, i) => (
                <tr key={i}>
                  <td>{r.posting_date ? formatDate(r.posting_date) : ""}</td>
                  <td>{isolate(entryWord(r.entry_type, words))}</td>
                  <td className="num">{isolate(show(r.amount))}</td>
                  <td className="num">{isolate(show(r.balance))}</td>
                  <td>{isolate(r.remarks ?? "")}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {q.data?.meta?.truncated && <p className="muted">{t("Only the first movements are shown. Narrow the period in the Stored Value Statement report.")}</p>}
    </section>
  );
}

function TopUp({ account, onDone }: { account: string; onDone: () => void }) {
  const [amount, setAmount] = useState("");
  const [reason, setReason] = useState("");
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const go = async () => {
    if (busy) return;
    setBusy(true);
    setProblem(null);
    try {
      await act("StoredValueAccount", account, "adjust", topUpBody({ amount, reason }));
      toast(t("Topped up"));
      setAmount("");
      setReason("");
      onDone();
    } catch (e) {
      setProblem(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <section className="erp-panel" aria-labelledby="sv-top">
      <h2 id="sv-top" className="erp-card-title">{t("Top up")}</h2>
      <div className="erp-form-grid">
        <Field label={t("Amount or units")} htmlFor="sv-top-amount">
          {amountInput(amount, setAmount, { id: "sv-top-amount", label: t("Amount or units") })}
        </Field>
        <Field label={t("Reason")} htmlFor="sv-top-reason">
          <input id="sv-top-reason" className="input" value={reason} onChange={(e) => setReason(e.target.value)} />
        </Field>
      </div>
      {problem && <Alert tone="error">{isolate(problem)}</Alert>}
      <div className="erp-actions">
        <Button loading={busy} disabled={!amount.trim() || !reason.trim()} onClick={() => void go()}>
          {t("Top up")}
        </Button>
      </div>
    </section>
  );
}

/** Proves a code for one operation under one effect key; the code and PIN are the caller's to clear. */
const prove = (kind: Kind, operation: Parameters<typeof proveBody>[0], code: string, pin: string, key: string) => act<Proof>("StoredValueKind", kind.id, "prove", proveBody(operation, code, pin, key));

function HoldValue({ kind, onHeld }: { kind: Kind; onHeld: (h: Hold) => void }) {
  const [order, setOrder] = useState<string | null>(null);
  const [amount, setAmount] = useState("");
  const [code, setCode] = useState("");
  const [pin, setPin] = useState("");
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const go = async () => {
    if (busy || !order) return;
    setBusy(true);
    setProblem(null);
    const [c, p] = [code, pin];
    setCode("");
    setPin("");
    try {
      const key = newKey();
      const proof = await prove(kind, "hold", c, p, key);
      const body = holdBody({ order, amount, proof: proof.proof });
      if (carriesSecret(body)) throw new Error("a code must not be sent to an operation that is logged");
      const h = await act<Hold>("StoredValueKind", kind.id, "hold", body, key);
      onHeld(h);
      toast(t("Held until {when}", { when: formatDateTime(h.expires_at) }));
      setAmount("");
    } catch (e) {
      setProblem(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <section className="erp-panel" aria-labelledby="sv-hold">
      <h2 id="sv-hold" className="erp-card-title">{t("Hold for an order")}</h2>
      <p className="muted">{t("Sets an amount aside for a submitted sales order. It ends when the order is invoiced, cancelled, or after {hours} hours.", { hours: kind.hold_hours ?? 72 })}</p>
      <div className="erp-form-grid">
        <Field label={t("Sales order")} htmlFor="sv-hold-order">
          <LinkInput to="SalesOrder" id="sv-hold-order" value={order} onChange={setOrder} />
        </Field>
        <Field label={t("Amount")} htmlFor="sv-hold-amount">
          {amountInput(amount, setAmount, { id: "sv-hold-amount", label: t("Amount") })}
        </Field>
        <Secret id="sv-hold-code" label={t("Code")} value={code} onChange={setCode} hint={t("Asked again: it is never kept.")} />
        {kind.requires_pin && <Secret id="sv-hold-pin" label={t("PIN")} value={pin} onChange={setPin} />}
      </div>
      {problem && <Alert tone="error">{isolate(problem)}</Alert>}
      <div className="erp-actions">
        <Button loading={busy} disabled={!order || !amount.trim() || !code.trim() || (kind.requires_pin && !pin.trim())} onClick={() => void go()}>
          {t("Hold")}
        </Button>
      </div>
    </section>
  );
}

function Holds({ kind, holds, onReleased }: { kind: Kind; holds: Hold[]; onReleased: (hold: string) => void }) {
  const [busy, setBusy] = useState<string | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const [manual, setManual] = useState("");
  const release = async (hold: string) => {
    if (busy) return;
    setBusy(hold);
    setProblem(null);
    try {
      await act("StoredValueKind", kind.id, "release", { hold }, newKey());
      toast(t("Released"));
      onReleased(hold);
    } catch (e) {
      setProblem(errorMessage(e));
    } finally {
      setBusy(null);
    }
  };
  const now = Date.now();
  return (
    <section className="erp-panel" aria-labelledby="sv-holds">
      <h2 id="sv-holds" className="erp-card-title">{t("Holds made here")}</h2>
      {holds.length === 0 ? (
        <p className="muted">{t("A hold you make here is listed with the time it ends. A hold made elsewhere is released by its number.")}</p>
      ) : (
        <ul className="erp-list">
          {holds.map((h) => (
            <li key={h.hold}>
              {isolate(show(h.amount))} · {running(h, now) ? t("ends {when}", { when: formatDateTime(h.expires_at) }) : t("ended {when}", { when: formatDateTime(h.expires_at) })}{" "}
              <Button disabled={busy !== null} loading={busy === h.hold} onClick={() => void release(h.hold)}>
                {t("Release")}
              </Button>
            </li>
          ))}
        </ul>
      )}
      <div className="erp-bar">
        <Ctl label={t("Hold number")} htmlFor="sv-release">
          <input id="sv-release" className="input" value={manual} onChange={(e) => setManual(e.target.value)} />
        </Ctl>
        <Button disabled={!manual.trim() || busy !== null} onClick={() => void release(manual.trim()).then(() => setManual(""))}>
          {t("Release")}
        </Button>
      </div>
      {problem && <Alert tone="error">{isolate(problem)}</Alert>}
    </section>
  );
}

function Issue({ kind }: { kind: Kind }) {
  const [amount, setAmount] = useState("");
  const [reason, setReason] = useState("");
  const [holder, setHolder] = useState<string | null>(null);
  const [code, setCode] = useState("");
  const [pin, setPin] = useState("");
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const [done, setDone] = useState<{ account: string; balance: string } | null>(null);
  // One key for what this form issues: a second press, or a retry after a lost answer, issues one card.
  const [key, setKey] = useState(newKey);

  const go = async () => {
    if (busy) return;
    setBusy(true);
    setProblem(null);
    const [c, p] = [code, pin];
    setCode("");
    setPin("");
    try {
      const proof = await prove(kind, "issue", c, p, key);
      const body = issueBody({ amount, reason, holder, proof: proof.proof });
      if (carriesSecret(body)) throw new Error("a code must not be sent to an operation that is logged");
      const out = await act<{ account: string; entry: string; balance: string }>("StoredValueKind", kind.id, "issue", body, key);
      setDone({ account: out.account, balance: out.balance });
      setKey(newKey());
      setAmount("");
      setReason("");
      if (proof.code) {
        // A code the server made is shown once, here, and is in nothing the page keeps.
        await confirmDialog({
          title: t("The new card's code"),
          message: (
            <div className="erp-stack">
              <p>{t("Shown once: copy it now. It cannot be found again.")}</p>
              <p className="num">
                <code>{proof.code}</code>
              </p>
            </div>
          ),
          confirmLabel: t("Done"),
          noCancel: true,
        });
      }
    } catch (e) {
      setProblem(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  const units = kind.holds === "Units";
  return (
    <form
      className="erp-stack"
      autoComplete="off"
      onSubmit={(e) => {
        e.preventDefault();
        void go();
      }}
    >
      {kind.disabled && <Alert tone="warning">{t("This kind is switched off: nothing more is issued.")}</Alert>}
      <div className="erp-form-grid">
        <Field label={units ? t("Units") : t("Amount")} htmlFor="sv-issue-amount" required>
          {amountInput(amount, setAmount, { id: "sv-issue-amount", label: units ? t("Units") : t("Amount") })}
        </Field>
        <Field label={t("Reason")} htmlFor="sv-issue-reason" required hint={t("Why it is issued without a sale.")}>
          <input id="sv-issue-reason" className="input" value={reason} onChange={(e) => setReason(e.target.value)} />
        </Field>
        <Field label={t("Customer")} htmlFor="sv-issue-holder" hint={t("Empty: whoever has its code.")}>
          <LinkInput to="Customer" id="sv-issue-holder" value={holder} onChange={setHolder} />
        </Field>
        <Secret id="sv-issue-code" label={t("Code")} value={code} onChange={setCode} hint={t("Empty: the server makes one and shows it once.")} />
        {kind.requires_pin && <Secret id="sv-issue-pin" label={t("PIN")} value={pin} onChange={setPin} />}
      </div>
      {problem && <Alert tone="error">{isolate(problem)}</Alert>}
      {done && (
        <Alert tone="success">
          {t("Issued. Balance {balance}.", { balance: show(done.balance) })} <LinkTitle to="StoredValueAccount" id={done.account} />
        </Alert>
      )}
      <div className="erp-actions">
        <Button type="submit" variant="primary" loading={busy} disabled={!amount.trim() || !reason.trim() || kind.disabled}>
          {t("Issue")}
        </Button>
      </div>
    </form>
  );
}
