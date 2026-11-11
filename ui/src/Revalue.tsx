// Revaluing foreign currency balances at a period's end: pick the day, see what every bank,
// receivable and payable balance in another currency would be worth at that day's rate against what
// the books have, and post the difference in one step. The balances, the rates and the gains are
// the server's (it works them out as the draft is asked about, before anything is saved); it is
// posted as the day's unrealised exchange gain or loss and reversed the day after, because the real
// difference is booked when a balance is settled.

import { useQuery } from "@tanstack/react-query";
import { useMemo, useState } from "react";
import { Link, useNavigate, useSearchParams } from "react-router";
import { api, docPath, errorMessage, formatDate, todayIso, t, isolate } from "@ferrum/client";
import { Alert, Button, confirmDialog, Field, toast, useShellTitle, StatePanel } from "@ferrum/kit";
import { Loading, LoadFailed } from "./states";
import { can, LinkInput, LinkTitle, useMe, useMetaIndex } from "@ferrum/records";
import { cmp } from "./decimal";
import { show } from "./money";
import { bodyOf, defaultDay, type Draft, outcome, party } from "./revaluation";
import { AccountsTabs } from "./Tabs";

export function Revalue() {
  useShellTitle(t("Revalue foreign currency balances"));
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
    const word = result === "gain" ? t("a net gain of {net}", { net: show(net) }) : result === "loss" ? t("a net loss of {value}", { value: show(net.replace("-", "")) }) : t("nothing net");
    const back = reverse ? t(" It is reversed on {reversal_date}.", { reversal_date: formatDate(d.reversal_date) }) : t(" It is not reversed afterwards.");
    const ok = await confirmDialog({ title: t("Revalue at {date}?", { date: formatDate(date) }), message: t("{length} balances are carried at that day's rate: {word}.{back}", { length: d.rows.length, word, back }), confirmLabel: t("Post the revaluation") });
    if (!ok) return;
    setBusy(true);
    setProblem(null);
    let made: { id: string; version: number } | null = null;
    try {
      made = await api.post<{ id: string; version: number }>("/api/v1/ExchangeRevaluation", body);
      const done = await api.post<{ number?: string }>(docPath("ExchangeRevaluation", made.id, "submit"), {}, { ifMatch: made.version });
      toast((done.number ? t("Revalued at {date} ({number})", { date: formatDate(date), number: done.number }) : t("Revalued at {date}", { date: formatDate(date) })));
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
      <StatePanel kind="empty" title={t("Choose a company")}>{t("A revaluation is for one company at a time.")}</StatePanel>
    );
  } else if (q.error) {
    table = (
      <>
        <LoadFailed error={q.error} onRetry={() => void q.refetch()} title={t("Could not work out the balances")} />
        <p className="muted">
          {t("A day with no exchange rate cannot be revalued.")} <Link to="/e/ExchangeRate/new">{t("Add an exchange rate")}</Link>
        </p>
      </>
    );
  } else if (!d) table = <Loading label={t("Working out the balances")} />;
  else if (d.rows.length === 0) {
    table = (
      <StatePanel kind="empty" title={t("Nothing to revalue on this day")}>{t("Every balance in another currency already stands at that day's rate, or there are none. Revaluing again on the same day finds nothing.")}</StatePanel>
    );
  } else {
    table = (
      <div className="table-wrap erp-statement">
        <table className="table">
          <thead>
            <tr>
              <th scope="col">{t("Account")}</th>
              <th scope="col">{t("Party")}</th>
              <th scope="col">{t("Currency")}</th>
              <th scope="col" className="num">
                {t("Balance")}
              </th>
              <th scope="col" className="num">
                {t("In the books")}
              </th>
              <th scope="col" className="num">
                {t("Rate")}
              </th>
              <th scope="col" className="num">
                {t("Revalued")}
              </th>
              <th scope="col" className="num">
                {t("Gain (loss)")}
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
                  <td className="num">{isolate(show(r.balance))}</td>
                  <td className="num">{isolate(show(r.book_value))}</td>
                  <td className="num">{isolate(show(r.exchange_rate))}</td>
                  <td className="num">{isolate(show(r.revalued))}</td>
                  <td className={`num ${cmp(r.gain_loss, "0") < 0 ? "erp-negative" : ""}`}>{isolate(show(r.gain_loss))}</td>
                </tr>
              );
            })}
          </tbody>
          <tfoot>
            <tr>
              <td colSpan={7}>{result === "gain" ? t("Net gain") : result === "loss" ? t("Net loss") : t("Net")}</td>
              <td className={`num ${result === "loss" ? "erp-negative" : ""}`}>{isolate(show(net))}</td>
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
          <p className="page-sub">{t("Carry bank, receivable and payable balances in other currencies at the rate of a day.")}</p>
        </div>
        <div className="erp-tools">
          <Link className="btn btn-default btn-md" to="/e/ExchangeRevaluation">
            {t("Earlier revaluations")}
          </Link>
        </div>
      </header>
      <form className="erp-filters" onSubmit={(e) => e.preventDefault()} aria-label={t("What to revalue")}>
        <Field label={t("Company")} htmlFor="rv-company">
          <LinkInput to="Company" id="rv-company" value={company} onChange={(id) => put({ company: id })} />
        </Field>
        <Field label={t("Revalue at")} htmlFor="rv-date" hint={t("Usually a period's last day.")}>
          <input id="rv-date" className="input" type="date" value={date} max={todayIso()} onChange={(e) => put({ date: e.target.value || null, reversal: null })} />
        </Field>
        <label className="check-field erp-check">
          <input type="checkbox" checked={reverse} onChange={(e) => put({ reverse: e.target.checked ? null : "0" })} />
          <span>{t("Reverse it afterwards")}</span>
        </label>
        {reverse && (
          <Field label={t("Reverse on")} htmlFor="rv-reversal" hint={t("The day after, when empty.")}>
            <input id="rv-reversal" className="input" type="date" value={reversalDate || d?.reversal_date || ""} min={date} onChange={(e) => put({ reversal: e.target.value || null })} />
          </Field>
        )}
        <Field label={t("Gain or loss goes to")} htmlFor="rv-account" hint={t("The company's exchange account, when empty.")}>
          <LinkInput to="Account" id="rv-account" value={account} linkFilters={{ is_group: false }} placeholder={t("The company's")} onChange={(id) => put({ account: id })} />
        </Field>
      </form>
      {problem && <Alert tone="error">{isolate(problem)}</Alert>}
      {table}
      <div className="erp-sticky-foot">
        <span className="muted" aria-live="polite">
          {!mayPost
            ? t("You can look at what a revaluation would do. An Accounts Manager posts it.")
            : d && d.rows.length > 0
              ? (d.rows.length === 1 ? t("{length} balance. The gain is unrealised: the real difference is booked when a balance is settled.", { length: d.rows.length }) : t("{length} balances. The gain is unrealised: the real difference is booked when a balance is settled.", { length: d.rows.length }))
              : ""}
        </span>
        <Button variant="primary" loading={busy} disabled={!mayPost || !d || d.rows.length === 0} onClick={() => void post()}>
          {t("Post the revaluation")}
        </Button>
      </div>
    </div>
  );
}
