// The two pages of a record that has a desk: the desk and the record's own form, side by side as tabs.

import { Link } from "react-router";
import { isolate, t } from "@ferrum/client";
import { formPath, recordPath } from "./desks";

export function DeskTabs({ entity, id, current }: { entity: string; id: string; current: "desk" | "form" }) {
  const tabs = [
    { key: "desk" as const, label: t("Desk"), to: recordPath(entity, id) },
    { key: "form" as const, label: t("Record"), to: formPath(entity, id) },
  ];
  return (
    <nav className="erp-tabs" aria-label={t("This record")}>
      {tabs.map((x) => (
        <Link key={x.key} to={x.to} aria-current={current === x.key ? "page" : undefined}>
          {isolate(x.label)}
        </Link>
      ))}
    </nav>
  );
}
