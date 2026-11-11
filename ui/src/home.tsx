// ERP's home (its modules), loaded when first opened: the first script holds only the frame
// (Ferrum docs/plan/apps-ui.md §12.9). For people who keep the books, a line above the modules says
// what needs them today and leads to the overview.

import { useQuery } from "@tanstack/react-query";
import { Link } from "react-router";
import { api, todayIso, t, isolate } from "@ferrum/client";
import { Icon } from "@ferrum/kit";
import { can, useMe, useMetaIndex } from "@ferrum/records";
import { ModulesHome } from "@ferrum/records/frame";
import { listOf, needing, type Overview, say, toneOf } from "./attention";

/** What needs the person today, when they can read the books and the bank: each count leads to the list it counts, the most pressing first. */
export function Needs() {
  const me = useMe().data;
  const entities = useMetaIndex().data?.entities;
  const mayRead = (name: string) => !!entities?.some((e) => e.name === name && can(e.permissions, "read"));
  const company = me?.default_company ?? null;
  const asOf = todayIso();
  const q = useQuery({
    queryKey: ["erp", "overview", company, asOf],
    enabled: !!company && mayRead("JournalEntry") && mayRead("BankTransaction"),
    queryFn: ({ signal }) => api.get<Overview>("/api/reports/finance_overview", { company, as_of: asOf }, signal),
  });
  // The overview is a courtesy here: if it cannot be had, the home is the modules, as it was.
  if (!q.data) return null;
  const items = needing(q.data);
  return (
    <aside className="erp-needs" aria-label={t("Needs you today")} data-calm={items.length === 0 || undefined}>
      {items.length === 0 ? (
        <span className="erp-needs-calm">
          <Icon name="check" size={14} />
          {t("Nothing needs attention")}
        </span>
      ) : (
        <ul className="erp-needs-list">
          {items.map((r) => (
            <li key={r.key} data-tone={toneOf(r)}>
              <Link to={listOf(r, company, asOf)}>{isolate(say(r))}</Link>
            </li>
          ))}
        </ul>
      )}
      <Link className="erp-needs-more" to="/r/finance_overview">
        {t("Open accounts today")}
      </Link>
    </aside>
  );
}

export function Home() {
  return (
    <>
      <Needs />
      <ModulesHome />
    </>
  );
}
