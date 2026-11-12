// The numbering desk (src/series.rs): the series of each kind of document with their templates, which
// are gapless and which is the default, the last number each gave, how to make one the default, and
// which series a given document took. The number a series gives next is the platform's counter's and
// no route shows it, so the last number given is shown instead.

import { useQueries, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Link, useSearchParams } from "react-router";
import { api, docPath, errorMessage, isolate, t } from "@ferrum/client";
import { Alert, Badge, Button, confirmDialog, StatePanel, TabPanel, Tabs, toast, useShellTitle } from "@ferrum/kit";
import { LinkInput, LinkTitle, useEntityMeta, useMetaIndex } from "@ferrum/records";
import { Ctl, Meta, PageHead } from "./chrome";
import { Gate, useRights } from "./desk";
import { Loading, LoadFailed } from "./states";
import { byKind, makeDefaultPlan, numberedBy, type Series, took } from "./series";

/** `/desk/numbering`. */
export function Numbering() {
  useShellTitle(t("Numbering series"));
  return (
    <Gate entity="NumberSeries">
      <Desk />
    </Gate>
  );
}

function Desk() {
  const [sp, setSp] = useSearchParams();
  const tab = sp.get("tab") ?? "series";
  return (
    <div className="erp-page">
      <PageHead
        title={t("Numbering series")}
        meta={<Meta items={[t("Which series numbers each kind of document, and which one a document took.")]} />}
        actions={<Link className="btn btn-primary btn-md" to="/e/NumberSeries/new">{t("New series")}</Link>}
      />
      <Tabs id="num" value={tab} onChange={(x) => setSp(x === "series" ? {} : { tab: x }, { replace: true })} tabs={[{ id: "series", label: t("Series") }, { id: "document", label: t("A document's series") }]} />
      <TabPanel id="num" tab={tab}>{tab === "document" ? <Which /> : <List />}</TabPanel>
    </div>
  );
}

function useKindName() {
  const idx = useMetaIndex().data;
  return (kind: string) => idx?.entities.find((e) => e.name === kind)?.label ?? kind;
}

function List() {
  const qc = useQueryClient();
  const may = useRights("NumberSeries").may;
  const name = useKindName();
  const meta = useEntityMeta("NumberSeries").data;
  const q = useQuery({
    queryKey: ["erp", "series"],
    queryFn: ({ signal }) => api.get<{ rows: Series[] }>(docPath("NumberSeries"), { fields: "id,version,series_name,document,company,template,gapless,use_by_default,disabled", limit: 200, sort: "series_name asc" }, signal),
  });
  const rows = q.data?.rows ?? [];
  // The last number each series gave: the newest document that took it.
  const last = useQueries({
    queries: rows.map((s) => ({
      queryKey: ["erp", "series-last", s.id],
      queryFn: ({ signal }: { signal: AbortSignal }) => api.get<{ rows: { number?: string | null }[] }>(docPath(s.document), { filters: numberedBy(s.id), fields: "id,number", sort: "created_at desc", limit: 1 }, signal),
    })),
  });
  const [busy, setBusy] = useState<string | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const makeDefault = async (s: Series) => {
    if (busy) return;
    const ok = await confirmDialog({ title: t("Use {name} by default?", { name: s.series_name }), message: t("Documents of this kind that name no series take it from now on."), confirmLabel: t("Use by default") });
    if (!ok) return;
    setBusy(s.id);
    setProblem(null);
    const plan = makeDefaultPlan(rows, s);
    const done: typeof plan = [];
    try {
      for (const w of plan) {
        await api.patch(docPath("NumberSeries", w.id), { use_by_default: w.use_by_default }, { ifMatch: w.version });
        done.push(w);
      }
      toast(t("{name} is now the default", { name: s.series_name }));
    } catch (e) {
      setProblem(errorMessage(e));
      // The one that stood is given back its place if the new one could not take it.
      const lost = done.find((w) => !w.use_by_default);
      if (lost) {
        const was = rows.find((r) => r.id === lost.id);
        await api.patch(docPath("NumberSeries", lost.id), { use_by_default: true }, { ifMatch: (was?.version ?? lost.version) + 1 }).catch(() => undefined);
      }
    } finally {
      await qc.invalidateQueries({ queryKey: ["erp", "series"] });
      setBusy(null);
    }
  };

  if (q.error) return <LoadFailed error={q.error} onRetry={() => void q.refetch()} title={t("Could not load the series")} />;
  if (!q.data) return <Loading columns={5} label={t("Loading the series")} />;
  const groups = byKind(rows, (meta?.fields.find((f) => f.name === "document")?.options as string[] | undefined) ?? []);
  if (groups.length === 0) {
    return (
      <StatePanel kind="empty" title={t("No series yet")} action={<Link className="btn btn-primary btn-md" to="/e/NumberSeries/new">{t("New series")}</Link>}>
        {t("Documents are numbered as their kind usually is until a series is made and used.")}
      </StatePanel>
    );
  }
  return (
    <div className="erp-stack">
      {problem && <Alert tone="error">{isolate(problem)}</Alert>}
      {groups.map((g) => (
        <section key={g.kind} className="erp-panel" aria-labelledby={`num-${g.kind}`}>
          <h2 id={`num-${g.kind}`} className="erp-card-title">{isolate(name(g.kind))}</h2>
          <div className="table-wrap">
            <table className="table" aria-label={t("Series for {kind}", { kind: name(g.kind) })}>
              <thead>
                <tr>
                  <th scope="col">{t("Series")}</th>
                  <th scope="col">{t("Template")}</th>
                  <th scope="col">{t("Company")}</th>
                  <th scope="col">{t("Last number given")}</th>
                  <th scope="col">{t("State")}</th>
                  <th scope="col"><span className="sr-only">{t("Default")}</span></th>
                </tr>
              </thead>
              <tbody>
                {g.series.map((s) => {
                  const i = rows.findIndex((r) => r.id === s.id);
                  const n = last[i]?.data?.rows[0]?.number;
                  return (
                    <tr key={s.id}>
                      <td><Link to={`/e/NumberSeries/${s.id}`}>{isolate(s.series_name)}</Link></td>
                      <td><code>{s.template}</code></td>
                      <td>{s.company ? <LinkTitle to="Company" id={s.company} /> : <span className="muted">{t("Every company")}</span>}</td>
                      <td>{last[i]?.isLoading ? "…" : n ? isolate(n) : <span className="muted">{t("None yet")}</span>}</td>
                      <td>
                        {s.use_by_default && <Badge tone="green">{t("Default")}</Badge>} {s.gapless && <Badge tone="blue">{t("Gapless")}</Badge>} {s.disabled && <Badge>{t("Switched off")}</Badge>}
                      </td>
                      <td>
                        {!s.use_by_default && !s.disabled && (
                          <Button disabled={!may("write") || busy !== null} loading={busy === s.id} onClick={() => void makeDefault(s)}>{t("Use by default")}</Button>
                        )}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        </section>
      ))}
      <p className="muted">{t("The number a series gives next is kept by the platform's counter and is not shown; the last number given is the newest document that took the series.")}</p>
    </div>
  );
}

function Which() {
  const name = useKindName();
  const meta = useEntityMeta("NumberSeries").data;
  const [sp, setSp] = useSearchParams({ tab: "document" });
  const kind = sp.get("kind");
  const id = sp.get("doc");
  const put = (k: string, v: string | null) => setSp((p) => { const n = new URLSearchParams(p); v ? n.set(k, v) : n.delete(k); if (k === "kind") n.delete("doc"); return n; }, { replace: true });
  const kinds = (meta?.fields.find((f) => f.name === "document")?.options as string[] | undefined) ?? [];
  const q = useQuery({ queryKey: ["erp", "series-doc", kind, id], enabled: !!kind && !!id, queryFn: ({ signal }) => api.get<{ number?: string | null; number_series?: string | null }>(docPath(kind ?? "", id ?? ""), undefined, signal) });
  const r = q.data ? took(q.data) : null;
  return (
    <div className="erp-stack">
      <form className="erp-bar" onSubmit={(e) => e.preventDefault()} aria-label={t("Which document")}>
        <Ctl label={t("Kind")} htmlFor="num-kind">
          <select id="num-kind" className="select" value={kind ?? ""} onChange={(e) => put("kind", e.target.value || null)}>
            <option value="">{t("Choose a kind")}</option>
            {kinds.map((k) => <option key={k} value={k}>{name(k)}</option>)}
          </select>
        </Ctl>
        {kind && (
          <Ctl label={t("Document")} htmlFor="num-doc">
            <LinkInput to={kind} id="num-doc" value={id} onChange={(v) => put("doc", v)} />
          </Ctl>
        )}
      </form>
      {!kind || !id ? (
        <StatePanel kind="empty" title={t("Choose a document")} />
      ) : q.error ? (
        <LoadFailed error={q.error} onRetry={() => void q.refetch()} title={t("Could not open the document")} />
      ) : !r ? (
        <Loading shape="form" label={t("Opening the document")} />
      ) : (
        <section className="erp-panel" aria-live="polite">
          <p>{r.number ? t("It is numbered {number}.", { number: r.number }) : t("It has no number yet: it is numbered when it is submitted.")}</p>
          {r.series ? (
            <p>{t("It took the series")} <LinkTitle to="NumberSeries" id={r.series} />.</p>
          ) : (
            <p className="muted">{t("It names no series: the series of its country's rules or the default of its kind numbers it when it is submitted, else its usual number.")}</p>
          )}
        </section>
      )}
    </div>
  );
}
