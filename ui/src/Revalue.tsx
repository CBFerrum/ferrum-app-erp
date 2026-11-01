// Revaluing foreign currency balances at a period's end: pick the day, see what every bank,
// receivable and payable balance in another currency would be worth at that day's rate against what
// the books have, and post the difference in one step. The balances, the rates and the gains are
// the server's (it works them out as the draft is asked about, before anything is saved); it is
// posted as the day's unrealised exchange gain or loss and reversed the day after, because the real
// difference is booked when a balance is settled.

import { useQuery } from "@tanstack/react-query";
import { useMemo, useState } from "react";
import { Link, useNavigate, useSearchParams } from "react-router";
import { api, docPath, errorMessage, formatDate, todayIso } from "@ferrum/client";
import { Alert, Button, confirmDialog, EmptyState, ErrorState, Field, PageLoading, toast, useShellTitle } from "@ferrum/kit";
import { can, LinkInput, LinkTitle, useMe, useMetaIndex } from "@ferrum/records";
import { cmp } from "./decimal";
import { show } from "./money";
import { bodyOf, defaultDay, type Draft, outcome, party } from "./revaluation";
import { AccountsTabs } from "./Tabs";

export function Revalue() {
  useShellTitle("Revalue foreign currency balances");
  const navigate = useNavigate();
  const me = useMe().data;
  const perms = useMetaIndex().data?.entities.find((e) => e.name === "ExchangeRevaluation")?.permissions;
  const mayPost = can(perms, "create") && can(perms, "submit");
  const [sp, setSp] = useSearchParams();
  const company = sp.get("company") ?? me?.default_company ?? null;
  const date = sp.get("date") ?? defaultDay(todayIso());
  const reverse = sp.get("reverse") !== "0";
  const reversalDate = sp.get("reversal") ?? "";
  const account = sp.get("account");
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const put = (changes: Record<string, string | null>) =>
    setSp(
      (prev) => {
        const next = new URLSearchParams(prev);
        for (const [k, v] of Object.entries(changes)) v ? next.set(k, v) : next.delete(k);
        return next;
      },
      { replace: true },
    );
  const body = useMemo(() => (company ? bodyOf({ company, date, reverse, reversalDate, account }) : null), [company, date, reverse, reversalDate, account]);
  // The server works out the balances and what each gains or loses before anything is saved.
  const q = useQuery({
    queryKey: ["erp", "revalue", body],
    enabled: !!body && !!date,
    queryFn: ({ signal }) => api.post<Draft>("/api/v1/ExchangeRevaluation/_calc", body, { signal }),
  });
  const d = q.data;
  const net = d?.total_gain_loss ?? "0";
  const result = outcome(net);

  const post = async () => {
    if (!body || !d) return;
    const word = result === "gain" ? `a net gain of ${show(net)}` : result === "loss" ? `a net loss of ${show(net.replace("-", ""))}` : "nothing net";
    const back = reverse ? ` It is reversed on ${formatDate(d.reversal_date)}.` : " It is not reversed afterwards.";
    const ok = await confirmDialog({ title: `Revalue at ${formatDate(date)}?`, message: `${d.rows.length} balances are carried at that day's rate: ${word}.${back}`, confirmLabel: "Post the revaluation" });
    if (!ok) return;
    setBusy(true);
    setProblem(null);
    let made: { id: string; version: number } | null = null;
    try {
      made = await api.post<{ id: string; version: number }>("/api/v1/ExchangeRevaluation", body);
      const done = await api.post<{ number?: string }>(docPath("ExchangeRevaluation", made.id, "submit"), {}, { ifMatch: made.version });
      toast(`Revalued at ${formatDate(date)}${done.number ? ` (${done.number})` : ""}`);
      navigate(`/e/ExchangeRevaluation/${made.id}`);
    } catch (e) {
      // A draft that could not be posted is not left behind.
      if (made) await api.del(docPath("ExchangeRevaluation", made.id)).catch(() => undefined);
      setProblem(errorMessage(e));
      setBusy(false);
    }
  };

  let table;
  if (!company) {
    table = (
      <EmptyState title="Choose a company" icon="home">
        <p className="muted">A revaluation is for one company at a time.</p>
      </EmptyState>
    );
  } else if (q.error) {
    table = (
      <>
        <ErrorState error={q.error} onRetry={() => void q.refetch()} title="Could not work out the balances" />
        <p className="muted">
          A day with no exchange rate cannot be revalued. <Link to="/e/ExchangeRate/new">Add an exchange rate</Link>
        </p>
      </>
    );
  } else if (!d) table = <PageLoading label="Working out the balances" />;
  else if (d.rows.length === 0) {
    table = (
      <EmptyState title="Nothing to revalue on this day" icon="check">
        <p className="muted">Every balance in another currency already stands at that day's rate, or there are none. Revaluing again on the same day finds nothing.</p>
      </EmptyState>
    );
  } else {
    table = (
      <div className="table-wrap erp-statement">
        <table className="table">
          <thead>
            <tr>
              <th scope="col">Account</th>
              <th scope="col">Party</th>
              <th scope="col">Currency</th>
              <th scope="col" className="num">
                Balance
              </th>
              <th scope="col" className="num">
                In the books
              </th>
              <th scope="col" className="num">
                Rate
              </th>
              <th scope="col" className="num">
                Revalued
              </th>
              <th scope="col" className="num">
                Gain (loss)
              </th>
            </tr>
          </thead>
          <tbody>
            {d.rows.map((r, i) => {
              const who = party(r);
              return (
                <tr key={i}>
                  <td>
                    <LinkTitle to="Account" id={r.account} />
                  </td>
                  <td>{who ? <LinkTitle to={who.to} id={who.id} /> : ""}</td>
                  <td>
                    <LinkTitle to="Currency" id={r.currency} />
                  </td>
                  <td className="num">{show(r.balance)}</td>
                  <td className="num">{show(r.book_value)}</td>
                  <td className="num">{show(r.exchange_rate)}</td>
                  <td className="num">{show(r.revalued)}</td>
                  <td className={`num ${cmp(r.gain_loss, "0") < 0 ? "erp-negative" : ""}`}>{show(r.gain_loss)}</td>
                </tr>
              );
            })}
          </tbody>
          <tfoot>
            <tr>
              <td colSpan={7}>{result === "gain" ? "Net gain" : result === "loss" ? "Net loss" : "Net"}</td>
              <td className={`num ${result === "loss" ? "erp-negative" : ""}`}>{show(net)}</td>
            </tr>
          </tfoot>
        </table>
      </div>
    );
  }

  return (
    <div className="erp-page erp-revalue">
      <header className="erp-head">
        <div>
          <AccountsTabs />
          <p className="page-sub">Carry bank, receivable and payable balances in other currencies at the rate of a day.</p>
        </div>
        <div className="erp-tools">
          <Link className="btn btn-default btn-md" to="/e/ExchangeRevaluation">
            Earlier revaluations
          </Link>
        </div>
      </header>
      <form className="erp-filters" onSubmit={(e) => e.preventDefault()} aria-label="What to revalue">
        <Field label="Company" htmlFor="rv-company">
          <LinkInput to="Company" id="rv-company" value={company} onChange={(id) => put({ company: id })} />
        </Field>
        <Field label="Revalue at" htmlFor="rv-date" hint="Usually a period's last day.">
          <input id="rv-date" className="input" type="date" value={date} max={todayIso()} onChange={(e) => put({ date: e.target.value || null, reversal: null })} />
        </Field>
        <label className="check-field erp-check">
          <input type="checkbox" checked={reverse} onChange={(e) => put({ reverse: e.target.checked ? null : "0" })} />
          <span>Reverse it afterwards</span>
        </label>
        {reverse && (
          <Field label="Reverse on" htmlFor="rv-reversal" hint="The day after, when empty.">
            <input id="rv-reversal" className="input" type="date" value={reversalDate || d?.reversal_date || ""} min={date} onChange={(e) => put({ reversal: e.target.value || null })} />
          </Field>
        )}
        <Field label="Gain or loss goes to" htmlFor="rv-account" hint="The company's exchange account, when empty.">
          <LinkInput to="Account" id="rv-account" value={account} linkFilters={{ is_group: false }} placeholder="The company's" onChange={(id) => put({ account: id })} />
        </Field>
      </form>
      {problem && <Alert tone="error">{problem}</Alert>}
      {table}
      <div className="erp-sticky-foot">
        <span className="muted" aria-live="polite">
          {!mayPost
            ? "You can look at what a revaluation would do. An Accounts Manager posts it."
            : d && d.rows.length > 0
              ? `${d.rows.length} ${d.rows.length === 1 ? "balance" : "balances"}. The gain is unrealised: the real difference is booked when a balance is settled.`
              : ""}
        </span>
        <Button variant="primary" loading={busy} disabled={!mayPost || !d || d.rows.length === 0} onClick={() => void post()}>
          Post the revaluation
        </Button>
      </div>
    </div>
  );
}
