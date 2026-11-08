// The bank reconciliation workbench: a bank account's statement lines on the left, what the books
// have for the selected line on the right, and the three figures that say how far apart the bank
// and the books are, always in view. j and k walk the lines, Enter accepts the exact match, and
// Shift+A settles every exact match at once after showing them. What a line can become (a match,
// a payment, a journal) and every rule about money (what is left to clear, whether a pairing still
// holds) is the server's; a line leaves the list at once and comes back, with the reason, if the
// server refuses.

import { useQuery, useQueryClient } from "@tanstack/react-query";
import { type FormEvent, type KeyboardEvent, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Link, useSearchParams } from "react-router";
import { api, docPath, errorMessage, formatDate, formatDecimal, todayIso, t as tr, isolate } from "@ferrum/client";
import { Badge, Button, Dialog, EmptyState, ErrorState, Field, Icon, PageLoading, Spinner, TabPanel, Tabs as ModeTabs, toast, useLiveEntity, useShellTitle, placed } from "@ferrum/kit";
import { can, LinkInput, useMetaIndex } from "@ferrum/records";
import { type Book, canReconcile, clearsLine, enterTarget, exactMatches, isDeposit, leftToReconcile, type Line, type Match, others, problems, reconcileBody, step, tick, type Ticks, untick, why, type Workbench } from "./reconcile";
import { cmp, isZero } from "./decimal";
import ImportDialog from "./Import";
import { useNarrow } from "./narrow";
import { AccountsTabs } from "./Tabs";

interface Account {
  id: string;
  account_name: string;
  bank?: string | null;
  masked_number?: string | null;
  is_default?: boolean;
}

const ROOT = ["erp", "bank"] as const;

/** Money as the account writes it: grouped, with the account's fraction digits. */
const fmt = (value: unknown, precision: number) => formatDecimal(value, precision);

// ───────────────────────────── the figures ─────────────────────────────

function Figures({ wb }: { wb: Workbench }) {
  const f = wb.figures;
  const settled = isZero(f.unexplained);
  return (
    <section className="erp-figures" aria-label={tr("Where the bank and the books stand")}>
      <dl className="erp-figure-row">
        <div className="erp-figure">
          <dt>{tr("Bank statement")}</dt>
          <dd>{isolate(fmt(f.statement_balance, wb.precision))}</dd>
          <dd className="faint">{tr("as of")} {isolate(formatDate(wb.as_of))}</dd>
        </div>
        <div className="erp-figure">
          <dt>{tr("Your books")}</dt>
          <dd>{isolate(fmt(f.book_balance, wb.precision))}</dd>
          <dd className="faint">{isolate(wb.name ?? "")}</dd>
        </div>
        <div className="erp-figure" data-tone={settled ? "calm" : "alarm"}>
          <dt>{tr("Difference")}</dt>
          <dd>{isolate(fmt(f.difference, wb.precision))}</dd>
          <dd className={settled ? "faint" : "erp-negative"} role="status">
            {settled ? tr("All of it is explained below") : tr("{value} cannot be explained", { value: fmt(f.unexplained, wb.precision) })}
          </dd>
        </div>
      </dl>
      <details className="erp-explain">
        <summary>{tr("What makes up the difference")}</summary>
        <table className="table erp-explain-table">
          <tbody>
            <tr>
              <td>{tr("In the books, not at the bank yet: money in")}</td>
              <td className="num">{isolate(fmt(f.uncleared_deposits, wb.precision))}</td>
            </tr>
            <tr>
              <td>{tr("In the books, not at the bank yet: money out")}</td>
              <td className="num">{isolate(fmt(f.uncleared_payments, wb.precision))}</td>
            </tr>
            <tr>
              <td>{tr("On the statement, not in the books yet")}</td>
              <td className="num">{isolate(fmt(f.unrecorded, wb.precision))}</td>
            </tr>
            <tr>
              <td>{tr("The statement should say")}</td>
              <td className="num">{isolate(fmt(f.expected_bank_balance, wb.precision))}</td>
            </tr>
            <tr className="bold">
              <td>{tr("Unexplained")}</td>
              <td className="num">{isolate(fmt(f.unexplained, wb.precision))}</td>
            </tr>
          </tbody>
        </table>
      </details>
    </section>
  );
}

// ───────────────────────────── the statement lines ─────────────────────────────

function statusBadge(line: Line) {
  if (line.status === "Reconciled") return <Badge tone="green">{tr("Matched")}</Badge>;
  if (line.status === "Partially Reconciled") return <Badge tone="amber">{tr("Partly matched")}</Badge>;
  return line.match ? <Badge tone="blue">{tr("Match found")}</Badge> : null;
}

function LineList({ wb, selected, onSelect, onAcceptKey, onAcceptAll }: { wb: Workbench; selected: string | null; onSelect: (id: string) => void; onAcceptKey: (line: Line) => void; onAcceptAll: () => void }) {
  const box = useRef<HTMLUListElement>(null);
  const onKeyDown = (e: KeyboardEvent<HTMLUListElement>) => {
    if (e.metaKey || e.ctrlKey || e.altKey) return;
    const move = (delta: 1 | -1) => {
      e.preventDefault();
      const id = step(wb.lines, selected, delta);
      if (id) onSelect(id);
    };
    if (e.key === "j" || e.key === "ArrowDown") move(1);
    else if (e.key === "k" || e.key === "ArrowUp") move(-1);
    else if (e.key === "Enter") {
      const line = wb.lines.find((l) => l.id === selected);
      if (line && enterTarget(line)) {
        e.preventDefault();
        onAcceptKey(line);
      }
    } else if (e.key === "A") {
      e.preventDefault();
      onAcceptAll();
    }
  };
  // The selected line stays in view as the keys move through a long statement.
  useEffect(() => {
    if (selected) box.current?.querySelector(`[data-line="${CSS.escape(selected)}"]`)?.scrollIntoView({ block: "nearest" });
  }, [selected]);
  return (
    <ul ref={box} className="erp-lines" role="listbox" tabIndex={0} aria-label={tr("Statement lines")} aria-activedescendant={selected ? `line-${selected}` : undefined} onKeyDown={onKeyDown}>
      {wb.lines.map((l) => {
        const money = isDeposit(l) ? "in" : "out";
        return (
          <li
            key={l.id}
            id={`line-${l.id}`}
            data-line={l.id}
            role="option"
            aria-selected={l.id === selected}
            aria-label={`${formatDate(l.date)}, ${l.description || l.reference_number || "no description"}, ${money === "in" ? "money in" : "money out"} ${fmt(l.unallocated, wb.precision)}${l.match ? ", a match was found" : ""}`}
            className="erp-line"
            onMouseDown={(e) => {
              e.preventDefault();
              box.current?.focus();
            }}
            onClick={() => onSelect(l.id)}
          >
            <div className="erp-line-main">
              <span className="erp-line-desc">{l.description || l.reference_number || tr("No description")}</span>
              <span className="erp-line-amount num" data-money={money}>
                {isolate(fmt(l.amount, wb.precision))}
              </span>
            </div>
            <div className="erp-line-sub">
              <span>{isolate(formatDate(l.date))}</span>
              {l.reference_number && <span className="faint">{isolate(l.reference_number)}</span>}
              {l.status === "Partially Reconciled" && <span className="faint">{tr("{value} left", { value: fmt(l.unallocated, wb.precision) })}</span>}
              <span className="spacer" />
              {statusBadge(l)}
            </div>
            {l.balance != null && <div className="erp-line-balance faint num">{tr("Bank balance")} {isolate(fmt(l.balance, wb.precision))}</div>}
          </li>
        );
      })}
    </ul>
  );
}

// ───────────────────────────── what the books have for a line ─────────────────────────────

interface Suggestions {
  suggestions: Match[];
}

function Entry({ title, sub, reasons, remaining, ticked, amount, error, precision, onTick, onAmount }: { title: string; sub: string; reasons?: string[]; remaining: string; ticked: boolean; amount: string; error?: string; precision: number; onTick: (on: boolean) => void; onAmount: (v: string) => void }) {
  return (
    <li className="erp-entry" data-ticked={ticked || undefined}>
      <label className="erp-entry-pick">
        <input type="checkbox" checked={ticked} onChange={(e) => onTick(e.target.checked)} />
        <span className="erp-entry-text">
          <strong>{isolate(title)}</strong>
          <span className="faint">{isolate(sub)}</span>
          {reasons && (
            <span className="erp-reasons">
              {reasons.map((r) => (
                <Badge key={r} tone={r === "Exact amount" ? "green" : "neutral"}>
                  {isolate(r)}
                </Badge>
              ))}
            </span>
          )}
        </span>
      </label>
      <div className="erp-entry-amount">
        <span className="faint num">{tr("{remaining} left", { remaining: fmt(remaining, precision) })}</span>
        {ticked && (
          <>
            <input className="input num" inputMode="decimal" aria-label={tr("Amount to clear against {title}", { title })} aria-invalid={error ? true : undefined} value={amount} onChange={(e) => onAmount(e.target.value)} />
            {error && <span className="field-error">{isolate(error)}</span>}
          </>
        )}
      </div>
    </li>
  );
}

function MatchTab({ wb, line, books, busy, onReconcile }: { wb: Workbench; line: Line; books: Book[]; busy: boolean; onReconcile: (body: ReturnType<typeof reconcileBody>) => void }) {
  const [ticks, setTicks] = useState<Ticks>({});
  const q = useQuery({
    queryKey: [...ROOT, "suggestions", line.id, line.version],
    queryFn: ({ signal }) => api.post<Suggestions>(docPath("BankTransaction", line.id, "actions", "get_suggestions"), {}, { signal }),
  });
  const ranked = q.data?.suggestions ?? [];
  const rest = useMemo(() => others(books, line, new Set(ranked.map((m) => m.payment_entry))), [books, line, ranked]);
  const entries = useMemo(
    () => [
      ...ranked.map((m) => ({ id: m.payment_entry, remaining: m.remaining, kind: m.payment_document })),
      ...rest.map((b) => ({ id: b.payment_entry, remaining: b.remaining, kind: b.payment_document })),
    ],
    [ranked, rest],
  );
  const issues = problems(ticks, entries, line.unallocated);
  const left = leftToReconcile(line.unallocated, ticks);
  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (canReconcile(ticks, issues)) onReconcile(reconcileBody(ticks, entries));
  };
  const toggle = (id: string, remaining: string) => (on: boolean) => setTicks((t) => (on ? tick(t, { id, remaining }, line.unallocated) : untick(t, id)));
  return (
    <form className="stack erp-tight" onSubmit={submit}>
      {line.match && cmp(left, line.unallocated) === 0 && (
        <div className="erp-best">
          <div>
            <strong>{line.match.number ?? tr("An entry")}</strong> {tr("clears this line exactly.")}
            <div className="faint">
              {isolate([line.match.party, line.match.reference, formatDate(line.match.posting_date)].filter(Boolean).join(" · "))}
            </div>
          </div>
          <Button type="button" variant="primary" loading={busy} onClick={() => onReconcile({ vouchers: [{ payment_document: line.match!.payment_document, payment_entry: line.match!.payment_entry, allocated_amount: line.match!.remaining }] })}>
            {tr("Accept")} <span className="kbd">{tr("Enter")}</span>
          </Button>
        </div>
      )}
      {q.error ? (
        <ErrorState error={q.error} onRetry={() => void q.refetch()} title={tr("Could not look for matches")} />
      ) : q.isPending ? (
        <Spinner label={tr("Looking for matches")} />
      ) : (
        <>
          <h3 className="erp-group">{ranked.length ? tr("Entries that could clear it") : tr("Nothing in the books looks like this yet")}</h3>
          {ranked.length > 0 && (
            <ul className="erp-entries">
              {ranked.map((m) => (
                <Entry
                  key={m.payment_entry}
                  title={m.number ?? tr("Entry")}
                  sub={[m.party, m.reference, formatDate(m.posting_date)].filter(Boolean).join(" · ")}
                  reasons={why(m)}
                  remaining={m.remaining}
                  ticked={m.payment_entry in ticks}
                  amount={ticks[m.payment_entry] ?? ""}
                  error={issues.amounts[m.payment_entry]}
                  precision={wb.precision}
                  onTick={toggle(m.payment_entry, m.remaining)}
                  onAmount={(v) => setTicks((t) => ({ ...t, [m.payment_entry]: v }))}
                />
              ))}
            </ul>
          )}
          {rest.length > 0 && (
            <details className="erp-more">
              <summary>{tr("Everything else the bank has not cleared ({length})", { length: rest.length })}</summary>
              <ul className="erp-entries">
                {rest.map((b) => (
                  <Entry
                    key={b.payment_entry}
                    title={b.number ?? tr("Entry")}
                    sub={[b.party, b.reference, formatDate(b.posting_date)].filter(Boolean).join(" · ")}
                    remaining={b.remaining}
                    ticked={b.payment_entry in ticks}
                    amount={ticks[b.payment_entry] ?? ""}
                    error={issues.amounts[b.payment_entry]}
                    precision={wb.precision}
                    onTick={toggle(b.payment_entry, b.remaining)}
                    onAmount={(v) => setTicks((t) => ({ ...t, [b.payment_entry]: v }))}
                  />
                ))}
              </ul>
            </details>
          )}
        </>
      )}
      <div className="erp-sticky-foot">
        <span aria-live="polite">
          {Object.keys(ticks).length === 0 ? tr("Tick the entries this line pays off.") : issues.total ? <span className="erp-negative">{isolate(issues.total)}</span> : clearsLine(line.unallocated, ticks) ? tr("That clears the line.") : tr("{left} of the line would be left.", { left: fmt(left, wb.precision) })}
        </span>
        <Button type="submit" variant={line.match ? "default" : "primary"} loading={busy} disabled={!canReconcile(ticks, issues)}>
          {tr("Reconcile")}
        </Button>
      </div>
    </form>
  );
}

function PaymentTab({ line, precision, busy, onMake }: { line: Line; precision: number; busy: boolean; onMake: (body: { party: string; allocate_invoices: boolean }) => void }) {
  const deposit = isDeposit(line);
  const [party, setParty] = useState<string | null>(null);
  const [settle, setSettle] = useState(false);
  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (party) onMake({ party, allocate_invoices: settle });
  };
  return (
    <form className="stack erp-tight" onSubmit={submit}>
      <p className="muted">
        {placed(tr("Makes {p1} for the {p2} still to reconcile, and matches it with this line."), { p1: deposit ? tr("a payment received from a customer") : tr("a payment made to a supplier"), p2: isolate(fmt(line.unallocated, precision)) })}
      </p>
      <Field label={deposit ? tr("Customer") : tr("Supplier")} htmlFor="bk-party" required>
        <LinkInput to={deposit ? "Customer" : "Supplier"} id="bk-party" value={party} required onChange={setParty} />
      </Field>
      <label className="check-field">
        <input type="checkbox" checked={settle} onChange={(e) => setSettle(e.target.checked)} />
        <span>{tr("Pay off their oldest open invoices with it")}</span>
      </label>
      <div className="erp-sticky-foot">
        <span className="faint">{tr("Without that it waits as an advance, to be applied later.")}</span>
        <Button type="submit" variant="primary" loading={busy} disabled={!party}>
          {tr("Make the payment")}
        </Button>
      </div>
    </form>
  );
}

function JournalTab({ line, precision, busy, onMake }: { line: Line; precision: number; busy: boolean; onMake: (body: { account: string; cost_center: string | null; project: string | null; remark: string | null }) => void }) {
  const [account, setAccount] = useState<string | null>(null);
  const [costCenter, setCostCenter] = useState<string | null>(null);
  const [project, setProject] = useState<string | null>(null);
  const [remark, setRemark] = useState("");
  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (account) onMake({ account, cost_center: costCenter, project, remark: remark.trim() || null });
  };
  return (
    <form className="stack erp-tight" onSubmit={submit}>
      <p className="muted">
        {placed(tr("Books the {p1} between the bank and another account: bank charges, interest, a transfer. The entry is made and matched with this line."), { p1: isolate(fmt(line.unallocated, precision)) })}
      </p>
      <Field label={tr("Other account")} htmlFor="bk-account" required hint={tr("The bank's own account is not offered.")}>
        <LinkInput to="Account" id="bk-account" value={account} required linkFilters={{ is_group: false }} onChange={setAccount} />
      </Field>
      <div className="erp-two">
        <Field label={tr("Cost centre")} htmlFor="bk-cc">
          <LinkInput to="CostCenter" id="bk-cc" value={costCenter} linkFilters={{ is_group: false }} onChange={setCostCenter} />
        </Field>
        <Field label={tr("Project")} htmlFor="bk-project">
          <LinkInput to="Project" id="bk-project" value={project} onChange={setProject} />
        </Field>
      </div>
      <Field label={tr("Remark")} htmlFor="bk-remark" hint={tr("The line's reference and description when left empty.")}>
        <input id="bk-remark" className="input" value={remark} onChange={(e) => setRemark(e.target.value)} />
      </Field>
      <div className="erp-sticky-foot">
        <span />
        <Button type="submit" variant="primary" loading={busy} disabled={!account}>
          {tr("Make the journal entry")}
        </Button>
      </div>
    </form>
  );
}

function ClearedTab({ wb, line, busy, onUndo }: { wb: Workbench; line: Line; busy: boolean; onUndo: (entry: string | null) => void }) {
  return (
    <div className="stack erp-tight">
      <p className="muted">{tr("This line has been matched with:")}</p>
      <ul className="erp-entries">
        {line.allocations.map((a) => (
          <li key={a.payment_entry} className="erp-entry">
            <div className="erp-entry-text">
              <strong>{a.payment_number ?? tr("Entry")}</strong>
              <span className="faint">{a.posting_date ? formatDate(a.posting_date) : ""}</span>
            </div>
            <div className="erp-entry-amount">
              <span className="num">{isolate(fmt(a.allocated_amount, wb.precision))}</span>
              <Button size="sm" loading={busy} onClick={() => onUndo(a.payment_entry)}>
                {tr("Undo")}
              </Button>
            </div>
          </li>
        ))}
      </ul>
      {line.allocations.length > 1 && (
        <Button loading={busy} onClick={() => onUndo(null)}>
          {tr("Undo all")}
        </Button>
      )}
    </div>
  );
}

function Panel({ wb, line, onBack, narrow, busy, act }: { wb: Workbench; line: Line; onBack: () => void; narrow: boolean; busy: boolean; act: Actions }) {
  const [mode, setMode] = useState("match");
  const open = !isZero(line.unallocated);
  return (
    <section className="erp-panel" aria-label={tr("The selected line")}>
      {narrow && (
        <Button size="sm" variant="ghost" icon="left" onClick={onBack}>
          {tr("All lines")}
        </Button>
      )}
      <header className="erp-panel-head">
        <div className="erp-panel-title">
          <h2>{line.description || line.reference_number || tr("Statement line")}</h2>
          <span className="faint">{isolate([formatDate(line.date), line.reference_number, line.number].filter(Boolean).join(" · "))}</span>
        </div>
        <div className="erp-panel-amount num" data-money={isDeposit(line) ? "in" : "out"}>
          {isolate(fmt(line.amount, wb.precision))}
          {line.status === "Partially Reconciled" && <span className="faint"> {tr("· {value} left", { value: fmt(line.unallocated, wb.precision) })}</span>}
        </div>
      </header>
      {!open ? (
        <ClearedTab wb={wb} line={line} busy={busy} onUndo={(entry) => void act.undo(line, entry)} />
      ) : (
        <>
          <ModeTabs
            id="bk-mode"
            value={mode}
            onChange={setMode}
            tabs={[
              { id: "match", label: tr("Match") },
              { id: "payment", label: tr("Create payment") },
              { id: "journal", label: tr("Create journal") },
            ]}
          />
          <TabPanel id="bk-mode" tab={mode}>
            {mode === "match" ? (
              <MatchTab wb={wb} line={line} books={wb.books} busy={busy} onReconcile={(body) => void act.reconcile(line, body)} />
            ) : mode === "payment" ? (
              <PaymentTab line={line} precision={wb.precision} busy={busy} onMake={(body) => void act.make(line, "make_payment_entry", body, "payment")} />
            ) : (
              <JournalTab line={line} precision={wb.precision} busy={busy} onMake={(body) => void act.make(line, "make_journal_entry", body, "journal entry")} />
            )}
          </TabPanel>
          {line.allocations.length > 0 && (
            <details className="erp-more">
              <summary>{tr("Already cleared against ({length})", { length: line.allocations.length })}</summary>
              <ClearedTab wb={wb} line={line} busy={busy} onUndo={(entry) => void act.undo(line, entry)} />
            </details>
          )}
        </>
      )}
    </section>
  );
}

// ───────────────────────────── accept all ─────────────────────────────

function AcceptAll({ wb, onClose, onConfirm, busy }: { wb: Workbench; onClose: () => void; onConfirm: () => void; busy: boolean }) {
  const all = exactMatches(wb.lines);
  return (
    <Dialog
      open
      wide
      onClose={onClose}
      title={(all.lines.length === 1 ? tr("Accept {length} exact match", { length: all.lines.length }) : tr("Accept {length} exact matches", { length: all.lines.length }))}
      footer={
        <>
          <Button onClick={onClose}>{tr("Not yet")}</Button>
          <Button variant="primary" loading={busy} onClick={onConfirm} data-autofocus>
            {tr("Accept {length}", { length: all.lines.length })}
          </Button>
        </>
      }
    >
      <p className="muted">
        {placed(tr("Each line below has an entry in the books for exactly its amount. Money in {p1}, money out {p2}. If one is wrong, close this and choose for it by hand."), { p1: isolate(fmt(all.money_in, wb.precision)), p2: isolate(fmt(all.money_out, wb.precision)) })}
      </p>
      <div className="table-wrap erp-preview">
        <table className="table">
          <thead>
            <tr>
              <th>{tr("Statement line")}</th>
              <th className="num">{tr("Amount")}</th>
              <th>{tr("Entry in the books")}</th>
            </tr>
          </thead>
          <tbody>
            {all.lines.map((l) => (
              <tr key={l.id}>
                <td>
                  {l.description || l.reference_number || tr("No description")}
                  <div className="faint">{isolate(formatDate(l.date))}</div>
                </td>
                <td className="num">{isolate(fmt(l.unallocated, wb.precision))}</td>
                <td>
                  {isolate(l.match!.number ?? "")}
                  <div className="faint">{isolate([l.match!.party, formatDate(l.match!.posting_date)].filter(Boolean).join(" · "))}</div>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </Dialog>
  );
}

// ───────────────────────────── the screen ─────────────────────────────

type Actions = {
  reconcile: (line: Line, body: ReturnType<typeof reconcileBody>) => Promise<void>;
  make: (line: Line, action: "make_payment_entry" | "make_journal_entry", body: object, what: string) => Promise<void>;
  undo: (line: Line, entry: string | null) => Promise<void>;
};

export function Bank() {
  useShellTitle(tr("Bank reconciliation"));
  const qc = useQueryClient();
  const meta = useMetaIndex().data;
  const narrow = useNarrow();
  const [sp, setSp] = useSearchParams();
  const asOf = sp.get("as_of") || todayIso();
  const all = sp.get("all") === "1";
  const lineId = sp.get("line");
  const [busy, setBusy] = useState(false);
  const [importing, setImporting] = useState(false);
  const [accepting, setAccepting] = useState(false);
  const [done, setDone] = useState<{ line: string; text: string } | null>(null);

  const accounts = useQuery({
    queryKey: [...ROOT, "accounts"],
    queryFn: () =>
      api
        .get<{ rows: Account[] }>("/api/v1/BankAccount", { filters: JSON.stringify({ disabled: false }), fields: "account_name,bank,masked_number,is_default", sort: "account_name asc", limit: 100 })
        .then((r) => r.rows),
  });
  const account = sp.get("account") ?? accounts.data?.find((a) => a.is_default)?.id ?? accounts.data?.[0]?.id ?? null;
  const key = useMemo(() => [...ROOT, "workbench", account, asOf, all] as const, [account, asOf, all]);
  const q = useQuery({
    queryKey: key,
    enabled: !!account,
    queryFn: ({ signal }) => api.post<Workbench>(docPath("BankAccount", account!, "actions", "workbench"), { as_of: asOf, reconciled: all }, { signal }),
  });
  const refresh = useCallback(() => void qc.invalidateQueries({ queryKey: ROOT }), [qc]);
  useLiveEntity("BankTransaction", refresh);
  useLiveEntity("PaymentEntry", refresh);

  const wb = q.data;
  const open = useMemo(() => wb?.lines.filter((l) => !isZero(l.unallocated)) ?? [], [wb]);
  const selected = wb?.lines.find((l) => l.id === lineId) ?? null;
  const canImport = can(meta?.entities.find((e) => e.name === "BankAccount")?.permissions, "write");

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
  const select = useCallback((id: string | null) => put({ line: id }), [put]);

  // A first look lands on the first line, on a screen wide enough to show it beside the list.
  useEffect(() => {
    if (!narrow && wb && !selected && wb.lines.length) select(wb.lines[0].id);
  }, [narrow, wb, selected, select]);
  useEffect(() => {
    if (!done) return;
    const t = setTimeout(() => setDone(null), 12_000);
    return () => clearTimeout(t);
  }, [done]);

  /** The line to land on once this one is gone: the next, else the one before. */
  const after = (line: Line): string | null => {
    const down = step(open, line.id, 1);
    if (down && down !== line.id) return down;
    const up = step(open, line.id, -1);
    return up && up !== line.id ? up : null;
  };
  const leave = (line: Line) => {
    qc.setQueryData<Workbench>(key, (w) => w && { ...w, lines: all ? w.lines : w.lines.filter((l) => l.id !== line.id), counts: { ...w.counts, unreconciled: Math.max(0, w.counts.unreconciled - 1) } });
    select(after(line));
  };

  /** Runs a write on a line; the line leaves the list at once when it is cleared in full, and returns with the server's reason if it is refused. */
  const run = async (line: Line, call: () => Promise<unknown>, clears: boolean, said: string | null) => {
    setBusy(true);
    const before = qc.getQueryData<Workbench>(key);
    if (clears) leave(line);
    try {
      await call();
      if (said) setDone({ line: line.id, text: said });
    } catch (e) {
      if (before) qc.setQueryData(key, before);
      select(line.id);
      toast(errorMessage(e), "error");
    } finally {
      setBusy(false);
      refresh();
    }
  };

  const act: Actions = {
    reconcile: (line, body) =>
      run(
        line,
        () => api.post(docPath("BankTransaction", line.id, "actions", "reconcile"), body),
        clearsLine(line.unallocated, Object.fromEntries(body.vouchers.map((v) => [v.payment_entry, v.allocated_amount]))),
        line.description ? tr("Matched {description}", { description: line.description }) : tr("Matched the line"),
      ),
    make: (line, action, body, what) =>
      run(
        line,
        async () => {
          const made = await api.post<{ number?: string }>(docPath("BankTransaction", line.id, "actions", action), body);
          toast((what === "payment" ? tr("Made payment {number}", { number: made.number ?? "" }) : tr("Made journal entry {number}", { number: made.number ?? "" })).trim());
        },
        true,
        line.description ? tr("Matched {description}", { description: line.description }) : tr("Matched the line"),
      ),
    undo: async (line, entry) => {
      await run(line, () => api.post(docPath("BankTransaction", line.id, "actions", "unreconcile"), entry ? { payment_entry: entry } : {}), false, null);
      setDone(null);
    },
  };
  /** Takes back the last thing done, from the bar that offers it. */
  const undoLast = async () => {
    if (!done) return;
    setBusy(true);
    try {
      await api.post(docPath("BankTransaction", done.line, "actions", "unreconcile"), {});
      setDone(null);
      select(done.line);
    } catch (e) {
      toast(errorMessage(e), "error");
    } finally {
      setBusy(false);
      refresh();
    }
  };
  const acceptKey = (line: Line) => {
    const m = enterTarget(line);
    if (m) void act.reconcile(line, { vouchers: [{ payment_document: m.payment_document, payment_entry: m.payment_entry, allocated_amount: m.remaining }] });
  };
  const acceptAll = async () => {
    if (!wb) return;
    setBusy(true);
    try {
      const picked = exactMatches(wb.lines).lines;
      await api.post(docPath("BankAccount", wb.bank_account, "actions", "accept_matches"), {
        matches: picked.map((l) => ({ transaction: l.id, payment_document: l.match!.payment_document, payment_entry: l.match!.payment_entry })),
      });
      toast((picked.length === 1 ? tr("Matched {length} line", { length: picked.length }) : tr("Matched {length} lines", { length: picked.length })));
      setAccepting(false);
    } catch (e) {
      toast(errorMessage(e), "error");
    } finally {
      setBusy(false);
      refresh();
    }
  };

  const exact = wb ? exactMatches(wb.lines).lines.length : 0;
  const showPanel = !narrow || !!selected;
  const showList = !narrow || !selected;

  let body;
  if (accounts.error) body = <ErrorState error={accounts.error} onRetry={() => void accounts.refetch()} />;
  else if (accounts.isPending) body = <PageLoading label={tr("Loading your bank accounts")} />;
  else if (!account) {
    body = (
      <EmptyState title={tr("No bank accounts yet")} icon="file">
        <p className="muted">{tr("Add the account at your bank, tied to its account in the books, and bring its statement in here.")}</p>
        {can(meta?.entities.find((e) => e.name === "BankAccount")?.permissions, "create") && (
          <Link className="btn btn-primary btn-md" to="/e/BankAccount/new">
            <Icon name="plus" />
            {tr("Add a bank account")}
          </Link>
        )}
      </EmptyState>
    );
  } else if (q.error) body = <ErrorState error={q.error} onRetry={() => void q.refetch()} title={tr("Could not load the workbench")} />;
  else if (!wb) body = <PageLoading label={tr("Loading the statement")} />;
  else {
    body = (
      <>
        <Figures wb={wb} />
        <div className="erp-subbar">
          <span aria-live="polite">
            {open.length === 0
              ? tr("Nothing left to reconcile")
              : (open.length === 1 ? tr("{length} line to reconcile, {exact} with an exact match", { length: open.length, exact }) : tr("{length} lines to reconcile, {exact} with an exact match", { length: open.length, exact }))}
          </span>
          <span className="spacer" />
          <label className="check-field">
            <input type="checkbox" checked={all} onChange={(e) => put({ all: e.target.checked ? "1" : null, line: null })} />
            <span>{tr("Show matched lines")}</span>
          </label>
          <Button variant={exact ? "primary" : "default"} disabled={!exact} onClick={() => setAccepting(true)}>
            {exact ? (exact === 1 ? tr("Accept {exact} exact match", { exact }) : tr("Accept {exact} exact matches", { exact })) : tr("Accept exact matches")}
          </Button>
        </div>
        {done && (
          <div className="erp-done" role="status">
            <Icon name="check" size={14} />
            <span>{isolate(done.text)}.</span>
            <Button size="sm" variant="ghost" loading={busy} onClick={() => void undoLast()}>
              {tr("Undo")}
            </Button>
          </div>
        )}
        {wb.lines.length === 0 ? (
          <EmptyState title={tr("All caught up")} icon="check">
            <p className="muted">{placed(tr("Every statement line up to {p1} is matched. Bring in the next statement when you have it."), { p1: isolate(formatDate(wb.as_of)) })}</p>
            {canImport && (
              <Button variant="primary" icon="upload" onClick={() => setImporting(true)}>
                {tr("Import a statement")}
              </Button>
            )}
          </EmptyState>
        ) : (
          <div className="erp-bench" data-narrow={narrow || undefined}>
            {showList && (
              <div className="erp-list">
                <LineList wb={wb} selected={selected?.id ?? null} onSelect={(id) => select(id)} onAcceptKey={acceptKey} onAcceptAll={() => exact && setAccepting(true)} />
                <p className="faint erp-keys">
                  {placed(tr("{p1} {p2} move · {p3} accept the match · {p4}+{p5} accept every exact match"), { p1: <span className="kbd">j</span>, p2: <span className="kbd">k</span>, p3: <span className="kbd">{tr("Enter")}</span>, p4: <span className="kbd">{tr("Shift")}</span>, p5: <span className="kbd">A</span> })}
                </p>
                {(wb.truncated || wb.books_truncated) && <p className="faint">{tr("Only the first part of a long statement is shown. Narrow it with the date above.")}</p>}
              </div>
            )}
            {showPanel &&
              (selected ? (
                <Panel key={`${selected.id}-${selected.version}`} wb={wb} line={selected} narrow={narrow} onBack={() => select(null)} busy={busy} act={act} />
              ) : (
                <EmptyState title={tr("Choose a line")} icon="list">
                  <p className="muted">{tr("Pick a statement line to see what the books have for it.")}</p>
                </EmptyState>
              ))}
          </div>
        )}
        {accepting && <AcceptAll wb={wb} busy={busy} onClose={() => setAccepting(false)} onConfirm={() => void acceptAll()} />}
      </>
    );
  }

  return (
    <div className="erp-page erp-bank">
      <header className="erp-head">
        <div>
          <AccountsTabs />
          <p className="page-sub">{tr("Match what the bank says with what the books say.")}</p>
        </div>
        <div className="erp-tools">
          <label className="erp-pick">
            <span className="sr-only">{tr("Bank account")}</span>
            <select className="select" value={account ?? ""} disabled={!accounts.data?.length} onChange={(e) => put({ account: e.target.value, line: null })}>
              {(accounts.data ?? []).map((a) => (
                <option key={a.id} value={a.id}>
                  {isolate(a.account_name)}
                  {a.masked_number ? ` ${a.masked_number}` : ""}
                </option>
              ))}
            </select>
          </label>
          <label className="erp-pick">
            <span className="sr-only">{tr("Statement balance as of")}</span>
            <input className="input" type="date" value={asOf} max={todayIso()} onChange={(e) => put({ as_of: e.target.value || null, line: null })} />
          </label>
          {canImport && account && (
            <Button icon="upload" onClick={() => setImporting(true)}>
              {tr("Import a statement")}
            </Button>
          )}
        </div>
      </header>
      {body}
      {importing && account && (
        <ImportDialog
          account={account}
          onClose={() => setImporting(false)}
          onDone={() => {
            setImporting(false);
            refresh();
          }}
        />
      )}
    </div>
  );
}
