// The payment run desk (Finance, addons/finance/src/payment_run.rs). A run is made from filters,
// the server proposes the open bills due by the day and those whose early-payment discount is open
// on the pay date, a person reviews and edits the proposal, submits it (through the workspace's
// approval policy on Payment Run when it has one), and makes the payments once: one payment per
// supplier, each in its own savepoint, so a supplier that fails is shown with the server's reason
// and the rest are paid. Making the payments again pays only what is not paid yet, which is the
// retry. The bank's file and the remittance advice are kept by the server and shown here.

import { useQueries, useQuery, useQueryClient } from "@tanstack/react-query";
import { useMemo, useRef, useState } from "react";
import { Link, useNavigate, useParams } from "react-router";
import { api, docPath, download, errorMessage, formatDate, formatDateTime, isolate, newKey, t, todayIso } from "@ferrum/client";
import { Alert, Badge, Button, confirmDialog, StatePanel, toast, useShellTitle, type Tone } from "@ferrum/kit";
import { LinkInput, LinkTitle, useMe } from "@ferrum/records";
import { Ctl, Meta, PageHead } from "./chrome";
import { act, amountInput, Gate, SetUp, useMissing, useRights } from "./desk";
import { DeskTabs } from "./DeskTabs";
import { Loading, LoadFailed } from "./states";
import { show } from "./money";
import { createBody, edited, type Edits, failed, left, lineOf, patchBody, payments, problem, type Run, type RunLine, totals } from "./paymentRuns";

const STATE_TONE: Record<string, Tone> = { Paid: "green", Failed: "red", Proposed: "neutral" };
const RUN_TONE: Record<string, Tone> = { Proposed: "neutral", Approved: "blue", Paid: "green", "Partly Paid": "amber" };
const stateWord = (s: string) => (s === "Paid" ? t("Paid") : s === "Failed" ? t("Failed") : t("Not paid yet"));
const runWord = (s: string) => (s === "Approved" ? t("Approved") : s === "Paid" ? t("Paid") : s === "Partly Paid" ? t("Partly paid") : t("Proposed"));

interface Approval {
  status: "pending" | "approved" | "rejected" | "recalled" | "cancelled" | "failed";
  failure?: string | null;
  steps: { label?: string; state: string; approvers: string[] }[];
  decisions: { decider: string; outcome: "approve" | "reject"; comment?: string | null; at: string; void?: boolean }[];
}

interface Send {
  id: string;
  recipient: string | null;
  state: "queued" | "held" | "sent" | "delivered" | "failed" | "skipped";
  reason?: string | null;
  error?: string | null;
  created_at: string;
}
const SEND_TONE: Record<string, Tone> = { queued: "neutral", held: "amber", sent: "blue", delivered: "green", failed: "red", skipped: "neutral" };
const sendWord = (s: string) =>
  s === "queued" ? t("Queued") : s === "held" ? t("Held until quiet hours end") : s === "sent" ? t("Sent") : s === "delivered" ? t("Delivered") : s === "failed" ? t("Failed") : t("Skipped");

/** The page `/e/PaymentRun/new`: what to pay, then the server proposes and the desk opens on the proposal. */
export function PaymentRunNew() {
  useShellTitle(t("New payment run"));
  return (
    <Gate entity="PaymentRun">
      <Setup />
    </Gate>
  );
}

function Setup() {
  const navigate = useNavigate();
  const me = useMe().data;
  const may = useRights("PaymentRun").may;
  const [f, setF] = useState({
    company: "",
    posting_date: todayIso(),
    due_by: todayIso(),
    bank_account: "",
    mode_of_payment: "",
    bank: "",
    bank_file: "",
    supplier: "",
    supplier_group: "",
    minimum_amount: "",
    take_discounts: true,
  });
  const company = f.company || me?.default_company || "";
  const set = (k: keyof typeof f, v: string | boolean) => setF((x) => ({ ...x, [k]: v }));
  const [busy, setBusy] = useState(false);
  const [problemText, setProblem] = useState<string | null>(null);
  // One key for the draft this form makes: a double press or a retry after a lost answer makes one run.
  const draftKey = useRef(newKey());
  const ready = !!company && !!f.bank_account && !!f.posting_date && !!f.due_by;
  // A run is paid from a bank or cash account; with none made, the server would refuse every run.
  const noBank = useMissing("Account", [["account_type", "in", ["Bank", "Cash"]], ["is_group", "=", false]]);

  const propose = async () => {
    if (busy || !ready) return;
    setBusy(true);
    setProblem(null);
    let made: { id: string } | null = null;
    try {
      made = await api.post<{ id: string }>(docPath("PaymentRun"), createBody({ ...f, company }), { idempotencyKey: draftKey.current });
      await act("PaymentRun", made.id, "propose");
      navigate(`/desk/payment-run/${made.id}`);
    } catch (e) {
      // A draft that was made but not proposed opens as it is: nothing is lost, and the desk can propose again.
      if (made) navigate(`/desk/payment-run/${made.id}`);
      else {
        setProblem(errorMessage(e));
        setBusy(false);
      }
    }
  };

  if (noBank) return <SetUp title={t("No bank or cash account yet")} why={t("A payment run is paid from a bank or cash account, and this workspace has none. Make one, then come back.")} to="/e/Account/new" label={t("Make an account")} />;
  return (
    <div className="erp-page">
      <PageHead
        title={t("New payment run")}
        meta={<Meta items={[t("Choose which supplier bills to pay and on what day. The proposal can be changed before anything is paid.")]} />}
        actions={
          <Link className="btn btn-ghost btn-md" to="/e/PaymentRun">
            {t("Earlier runs")}
          </Link>
        }
      />
      <form className="erp-bar" onSubmit={(e) => e.preventDefault()} aria-label={t("What to pay")}>
        <Ctl label={t("Company")} htmlFor="pr-company">
          <LinkInput to="Company" id="pr-company" value={company} onChange={(id) => set("company", id ?? "")} />
        </Ctl>
        <Ctl label={t("Pay on")} htmlFor="pr-pay">
          <input id="pr-pay" className="input" type="date" value={f.posting_date} onChange={(e) => set("posting_date", e.target.value)} />
        </Ctl>
        <Ctl label={t("Due on or before")} htmlFor="pr-due">
          <input id="pr-due" className="input" type="date" value={f.due_by} onChange={(e) => set("due_by", e.target.value)} />
        </Ctl>
        <Ctl label={t("Pay from")} htmlFor="pr-bank">
          <LinkInput to="Account" id="pr-bank" value={f.bank_account} linkFilters={{ is_group: false, company }} onChange={(id) => set("bank_account", id ?? "")} />
        </Ctl>
        <label className="check-field erp-check">
          <input type="checkbox" checked={f.take_discounts} onChange={(e) => set("take_discounts", e.target.checked)} />
          <span>{t("Take early-payment discounts")}</span>
        </label>
      </form>
      <details className="erp-more">
        <summary>{t("More filters and the bank's file")}</summary>
        <div className="erp-bar">
          <Ctl label={t("Supplier")} htmlFor="pr-supplier">
            <LinkInput to="Supplier" id="pr-supplier" value={f.supplier} onChange={(id) => set("supplier", id ?? "")} />
          </Ctl>
          <Ctl label={t("Supplier group")} htmlFor="pr-group">
            <LinkInput to="SupplierGroup" id="pr-group" value={f.supplier_group} onChange={(id) => set("supplier_group", id ?? "")} />
          </Ctl>
          <Ctl label={t("Least to pay a supplier")} htmlFor="pr-least" title={t("A supplier whose total comes to less waits for a later run.")}>
            {amountInput(f.minimum_amount, (v) => set("minimum_amount", v), { id: "pr-least", label: t("Least to pay a supplier") })}
          </Ctl>
          <Ctl label={t("Mode of payment")} htmlFor="pr-mode">
            <LinkInput to="ModeOfPayment" id="pr-mode" value={f.mode_of_payment} onChange={(id) => set("mode_of_payment", id ?? "")} />
          </Ctl>
          <Ctl label={t("Bank account for the file")} htmlFor="pr-file-bank">
            <LinkInput to="BankAccount" id="pr-file-bank" value={f.bank} onChange={(id) => set("bank", id ?? "")} />
          </Ctl>
          <Ctl label={t("Bank file template")} htmlFor="pr-template" title={t("Empty: the workspace's usual one.")}>
            <input id="pr-template" className="input" value={f.bank_file} placeholder={t("The usual one")} onChange={(e) => set("bank_file", e.target.value)} />
          </Ctl>
        </div>
      </details>
      {problemText && <Alert tone="error">{isolate(problemText)}</Alert>}
      <div className="erp-sticky-foot">
        <span className="muted" aria-live="polite">
          {may("create") ? "" : t("You can look at payment runs. An Accounts Manager makes one.")}
        </span>
        <Button variant="primary" loading={busy} disabled={!ready || !may("create")} onClick={() => void propose()}>
          {t("Propose bills")}
        </Button>
      </div>
    </div>
  );
}

/** The page `/desk/payment-run/:id`: the proposal, its review, the approval, the payments and what they left. */
export function PaymentRunDesk() {
  const { id = "" } = useParams();
  useShellTitle(t("Payment run"));
  return (
    <Gate entity="PaymentRun">
      <Desk id={id} />
    </Gate>
  );
}

function Desk({ id }: { id: string }) {
  const qc = useQueryClient();
  const may = useRights("PaymentRun").may;
  const [edits, setEdits] = useState<Edits>({});
  const [busy, setBusy] = useState<null | "save" | "propose" | "submit" | "pay">(null);
  const [problemText, setProblem] = useState<string | null>(null);
  const q = useQuery({ queryKey: ["erp", "run", id], queryFn: ({ signal }) => api.get<Run>(docPath("PaymentRun", id), undefined, signal) });
  const run = q.data;
  const approval = useQuery({
    queryKey: ["erp", "run-approval", id],
    enabled: !!run,
    queryFn: ({ signal }) => api.get<Approval | null>(docPath("PaymentRun", id, "approval"), undefined, signal).catch(() => null),
  });
  const submitted = run?.docstatus === 1;
  const files = useQuery({
    queryKey: ["erp", "run-files", id, run?.paid_amount],
    enabled: submitted && !!run?.paid_amount && run.paid_amount !== "0",
    queryFn: ({ signal }) => api.get<{ attachments: { id: string; file_name: string; size_bytes: number; url: string }[] }>(docPath("PaymentRun", id, "attachments"), undefined, signal),
  });
  const made = useMemo(() => payments(run?.lines ?? []), [run]);
  const remits = useQueries({
    queries: made.map((p) => ({
      queryKey: ["erp", "remit", p],
      queryFn: ({ signal }: { signal: AbortSignal }) => api.get<{ sends: Send[] }>("/api/messages/log", { entity: "PaymentEntry", record_id: p }, signal),
    })),
  });

  const keep = (r: Run) => {
    qc.setQueryData(["erp", "run", id], r);
    setEdits({});
  };
  const run_ = async (what: NonNullable<typeof busy>, go: () => Promise<void>) => {
    if (busy) return;
    setBusy(what);
    setProblem(null);
    try {
      await go();
    } catch (e) {
      setProblem(errorMessage(e));
    } finally {
      setBusy(null);
    }
  };

  if (q.error) return <LoadFailed error={q.error} onRetry={() => void q.refetch()} title={t("Could not open the payment run")} />;
  if (!run) return <Loading label={t("Opening the payment run")} />;

  const draft = run.docstatus === 0;
  const lines = run.lines ?? [];
  const sums = totals(lines, edits);
  const bad = lines.map((l) => problem(lineOf(l, edits))).filter(Boolean).length;
  const code = run.currency_code ?? "";
  const money = (v: string | null | undefined) => isolate(show(v ?? "0"));
  const wrong = failed(lines);
  const pending = approval.data?.status === "pending";

  const save = async () =>
    run_("save", async () => {
      keep(await api.patch<Run>(docPath("PaymentRun", id), patchBody(lines, edits), { ifMatch: run.version }));
      toast(t("Saved"));
    });
  const proposeAgain = async () => {
    if (edited(edits) && !(await confirmDialog({ title: t("Propose again?"), message: t("Your changes to the proposal are lost: the bills are chosen again."), confirmLabel: t("Propose again"), danger: true }))) return;
    await run_("propose", async () => keep(await act<Run>("PaymentRun", id, "propose")));
  };
  const submit = async () => {
    const ok = await confirmDialog({
      title: t("Submit this run?"),
      message: t("{count} bills to {suppliers} suppliers, {pay} to pay. Where the workspace asks for approval, it is paid once the approvers agree.", { count: sums.count, suppliers: sums.suppliers, pay: show(sums.pay) }),
      confirmLabel: t("Submit"),
    });
    if (!ok) return;
    await run_("submit", async () => {
      let version = run.version;
      if (edited(edits)) {
        const saved = await api.patch<Run>(docPath("PaymentRun", id), patchBody(lines, edits), { ifMatch: version });
        keep(saved);
        version = saved.version;
      }
      const done = await api.post<Run & { _approval?: { status?: string } }>(docPath("PaymentRun", id, "submit"), {}, { ifMatch: version });
      keep(done);
      void qc.invalidateQueries({ queryKey: ["erp", "run-approval", id] });
      toast(done._approval?.status === "pending" ? t("Sent for approval before it is submitted") : t("Submitted"));
    });
  };
  const pay = async () => {
    const ok = await confirmDialog({
      title: wrong.length ? t("Pay the failed lines again?") : t("Make the payments?"),
      message: wrong.length ? t("The lines that failed are tried again. What is paid already is not paid again.") : t("Each supplier is paid its bills now. What is paid already is not paid again."),
      confirmLabel: wrong.length ? t("Pay again") : t("Make payments"),
    });
    if (!ok) return;
    await run_("pay", async () => {
      // A new key for each press: a double press is stopped by the button, and a press made on purpose to retry is a new request.
      const done = await act<Run>("PaymentRun", id, "make_payments", {}, newKey());
      keep(done);
      void qc.invalidateQueries({ queryKey: ["erp", "run-files", id] });
      toast(done.failed_count ? t("{count} lines were not paid", { count: done.failed_count }) : t("Paid"));
    });
  };

  const editLine = (l: RunLine, c: Edits[string]) => l.id && setEdits((e) => ({ ...e, [l.id as string]: { ...e[l.id as string], ...c } }));
  const mayEdit = draft && may("write");

  return (
    <div className="erp-page">
      <DeskTabs entity="PaymentRun" id={id} current="desk" />
      <PageHead
        title={run.number ? t("Payment run {number}", { number: run.number }) : t("Payment run (draft)")}
        meta={
          <Meta
            items={[
              <Badge key="s" tone={RUN_TONE[run.run_status] ?? "neutral"}>{runWord(run.run_status)}</Badge>,
              t("Pay on {date}", { date: formatDate(run.posting_date) }),
              t("Bills due by {date}", { date: formatDate(run.due_by) }),
              code && isolate(code),
            ]}
          />
        }
      />
      {approval.data && <ApprovalLine a={approval.data} />}
      <dl className="erp-figures">
        <div className="erp-figure-row erp-four">
          <div className="erp-figure">
            <dt>{t("To pay")}</dt>
            <dd>{money(sums.pay)}</dd>
            <dd className="faint">{t("{count} bills, {suppliers} suppliers", { count: sums.count, suppliers: sums.suppliers })}</dd>
          </div>
          <div className="erp-figure">
            <dt>{t("Discounts taken")}</dt>
            <dd>{money(sums.discount)}</dd>
          </div>
          <div className="erp-figure">
            <dt>{t("Paid so far")}</dt>
            <dd>{money(run.paid_amount)}</dd>
          </div>
          <div className="erp-figure" data-tone={run.failed_count ? "alarm" : undefined}>
            <dt>{t("Lines that failed")}</dt>
            <dd>{run.failed_count ?? 0}</dd>
          </div>
        </div>
      </dl>
      {problemText && <Alert tone="error">{isolate(problemText)}</Alert>}
      {lines.length === 0 ? (
        <StatePanel kind="empty" title={t("Nothing to pay")}>
          {t("No bill is due by that day, or none has an early-payment discount open on the pay date. Change the day or the filters, or propose again.")}
        </StatePanel>
      ) : (
        <div className="table-wrap">
          <table className="table" aria-label={t("Bills to pay")}>
            <thead>
              <tr>
                {mayEdit && <th scope="col">{t("Pay")}</th>}
                <th scope="col">{t("Supplier")}</th>
                <th scope="col">{t("Bill")}</th>
                <th scope="col">{t("Due")}</th>
                <th scope="col" className="num">{t("Outstanding")}</th>
                <th scope="col" className="num">{t("Discount taken")}</th>
                <th scope="col" className="num">{t("To pay")}</th>
                <th scope="col">{t("Instrument no.")}</th>
                {!draft && <th scope="col">{t("Result")}</th>}
              </tr>
            </thead>
            <tbody>
              {lines.map((raw, i) => {
                const l = lineOf(raw, edits);
                const why = problem(l);
                const name = `${l.supplier_name ?? ""} ${l.invoice_number ?? ""}`.trim();
                return (
                  <tr key={l.id ?? i} className={l.pay ? "" : "erp-off"}>
                    {mayEdit && (
                      <td>
                        <input type="checkbox" aria-label={t("Pay {name}", { name })} checked={l.pay} onChange={(e) => editLine(l, { pay: e.target.checked })} />
                      </td>
                    )}
                    <td>{l.supplier ? <LinkTitle to="Supplier" id={l.supplier} /> : isolate(l.supplier_name ?? "")}</td>
                    <td>{isolate(`${l.invoice_type ?? ""} ${l.invoice_number ?? ""}`.trim())}</td>
                    <td>{l.due_date ? formatDate(l.due_date) : ""}</td>
                    <td className="num">{money(l.outstanding)}</td>
                    <td className="num">
                      {mayEdit && l.pay ? amountInput(l.discount, (v) => editLine(l, { discount: v }), { label: t("Discount taken on {name}", { name }) }) : money(l.discount)}
                    </td>
                    <td className="num">
                      {mayEdit && l.pay ? amountInput(l.amount, (v) => editLine(l, { amount: v }), { label: t("To pay on {name}", { name }) }) : money(l.amount)}
                      {why && (
                        <p className="field-error" role="alert">
                          {why === "nothing" ? t("Pays nothing: untick it or give an amount.") : t("With the discount it pays more than is outstanding.")}
                        </p>
                      )}
                      {!why && l.pay && draft && left(l) !== "0" && <p className="muted">{t("{left} stays open", { left: show(left(l)) })}</p>}
                    </td>
                    <td>
                      {mayEdit && l.pay ? (
                        <input className="input" aria-label={t("Instrument number for {name}", { name })} value={l.instrument_no ?? ""} onChange={(e) => editLine(l, { instrument_no: e.target.value })} />
                      ) : (
                        isolate(l.instrument_no ?? "")
                      )}
                    </td>
                    {!draft && (
                      <td>
                        {l.pay ? (
                          <>
                            <Badge tone={STATE_TONE[l.state ?? "Proposed"] ?? "neutral"}>{stateWord(l.state ?? "Proposed")}</Badge>
                            {l.error && <p className="erp-negative">{isolate(l.error)}</p>}
                            {l.payment_entry && (
                              <p>
                                <Link to={`/e/PaymentEntry/${l.payment_entry}`}>{t("Payment")}</Link>
                              </p>
                            )}
                          </>
                        ) : (
                          t("Left out")
                        )}
                      </td>
                    )}
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
      {submitted && (files.data?.attachments.length ?? 0) > 0 && (
        <section className="erp-panel" aria-labelledby="pr-files">
          <h2 id="pr-files" className="erp-card-title">{t("Bank file")}</h2>
          <ul className="erp-list">
            {files.data!.attachments.map((f) => (
              <li key={f.id}>
                <Button icon="download" onClick={() => download(f.url, f.file_name).catch((e) => toast(errorMessage(e), "error"))}>
                  {isolate(f.file_name)}
                </Button>
              </li>
            ))}
          </ul>
        </section>
      )}
      {made.length > 0 && (
        <section className="erp-panel" aria-labelledby="pr-remit">
          <h2 id="pr-remit" className="erp-card-title">{t("Remittance advice")}</h2>
          {remits.some((r) => r.isLoading) && <Loading shape="table" columns={3} rows={2} label={t("Loading the remittance messages")} />}
          {remits.every((r) => !r.isLoading) && remits.every((r) => !r.data?.sends.length) && <p className="muted">{t("No remittance message has been queued yet.")}</p>}
          <ul className="erp-list">
            {remits.flatMap((r) => r.data?.sends ?? []).map((s) => (
              <li key={s.id}>
                {isolate(s.recipient ?? "")} <Badge tone={SEND_TONE[s.state] ?? "neutral"}>{sendWord(s.state)}</Badge> <time dateTime={s.created_at}>{formatDateTime(s.created_at)}</time>
                {(s.reason || s.error) && <span className="muted"> {isolate(s.reason || s.error || "")}</span>}
              </li>
            ))}
          </ul>
        </section>
      )}
      <div className="erp-sticky-foot">
        <span className="muted" aria-live="polite">
          {draft && !may("write")
            ? t("You can look at this run. Someone who may change payment runs reviews it.")
            : draft && bad > 0
              ? t("{count} lines need a change before the run can be saved.", { count: bad })
              : pending
                ? t("Waiting for approval: it can be paid once the approvers agree.")
                : ""}
        </span>
        {draft ? (
          <div className="erp-actions">
            <Button disabled={!mayEdit || busy !== null} loading={busy === "propose"} onClick={() => void proposeAgain()}>
              {t("Propose again")}
            </Button>
            <Button disabled={!mayEdit || !edited(edits) || bad > 0 || busy !== null} loading={busy === "save"} onClick={() => void save()}>
              {t("Save changes")}
            </Button>
            <Button variant="primary" disabled={!may("submit") || sums.count === 0 || bad > 0 || busy !== null} loading={busy === "submit"} onClick={() => void submit()}>
              {t("Submit")}
            </Button>
          </div>
        ) : submitted && run.run_status !== "Paid" ? (
          <Button variant="primary" disabled={!may("submit") || pending || busy !== null} loading={busy === "pay"} onClick={() => void pay()}>
            {wrong.length ? t("Pay the failed lines again") : t("Make payments")}
          </Button>
        ) : null}
      </div>
    </div>
  );
}

function ApprovalLine({ a }: { a: Approval }) {
  const tone = a.status === "approved" ? "success" : a.status === "pending" ? "warning" : a.status === "rejected" || a.status === "failed" ? "error" : "info";
  const word =
    a.status === "pending" ? t("Waiting for approval.") : a.status === "approved" ? t("Approved.") : a.status === "rejected" ? t("Rejected.") : a.status === "failed" ? t("Approved, but the submit was refused.") : a.status === "recalled" ? t("The request was recalled.") : t("The request was cancelled.");
  const open = a.steps.find((s) => s.state === "open");
  return (
    <Alert tone={tone}>
      {word}
      {open && open.approvers.length > 0 && (
        <>
          {" "}
          {t("Waiting for")}{" "}
          {open.approvers.map((u, i) => (
            <span key={u}>
              {i > 0 && ", "}
              <LinkTitle to="User" id={u} />
            </span>
          ))}
          .
        </>
      )}
      {a.failure && <> {isolate(a.failure)}</>}
      {a.decisions.filter((d) => !d.void && d.comment).map((d, i) => (
        <span key={i}> {isolate(d.comment ?? "")}</span>
      ))}
    </Alert>
  );
}
