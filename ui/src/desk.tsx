// What the desks (payment runs, stored value, promotions, tax returns, withholding, shipping,
// planning, numbering) share: the person's rights on what a desk works on, the call of a record's
// action, and the refusal as the server wrote it. A desk shows only where the add-on that serves it
// is installed and only to people who can read what it works on.

import type { ReactNode } from "react";
import { Link } from "react-router";
import { useQuery } from "@tanstack/react-query";
import { api, docPath, isolate, newKey, t } from "@ferrum/client";
import { StatePanel } from "@ferrum/kit";
import { can, useMe, useMetaIndex, type Perms } from "@ferrum/records";
import { Loading } from "./states";

export { newKey };

/** What the person may do with an entity; `known` is false until the metadata is in, `installed` false when no package serves it. */
export function useRights(entity: string): { known: boolean; installed: boolean; perms: Perms | undefined; may: (k: "read" | "write" | "create" | "submit" | "cancel") => boolean } {
  const q = useMetaIndex();
  const found = q.data?.entities.find((e) => e.name === entity);
  return { known: !!q.data, installed: !!found, perms: found?.permissions, may: (k) => can(found?.permissions, k) };
}

/** The add-on that brings an entity, for those that are not ERP's own packages (an absent entity has no metadata to say). */
const ADDON: Record<string, () => string> = { PaymentRun: () => t("Finance") };

/** Holds a desk's content back until the entity it works on is known to be installed and readable. */
export function Gate({ entity, children }: { entity: string; children: ReactNode }) {
  const r = useRights(entity);
  const admin = useMe().data?.user.is_admin;
  if (!r.known) return <Loading shape="form" label={t("Loading")} />;
  if (!r.installed) {
    const addon = ADDON[entity]?.();
    return (
      <StatePanel
        kind="empty"
        title={t("Not available here")}
        action={admin ? <a className="btn btn-primary btn-md" href="/store/">{t("Open the app store")}</a> : undefined}
      >
        {addon ? t("This screen comes with the {addon} add-on, which is not installed in this workspace.", { addon }) : t("The add-on that serves this screen is not installed in this workspace.")}{" "}
        {admin ? t("Install it from the app store.") : t("Ask an administrator to install it.")}
      </StatePanel>
    );
  }
  if (!r.may("read")) return <StatePanel kind="denied" />;
  return <>{children}</>;
}

/**
 * Whether a desk lacks what it works from: true once the answer is in and no record of `entity` matches
 * `filters`; a pending or failed question never says so, so the desk shows as before.
 */
export function useMissing(entity: string, filters?: unknown): boolean {
  const q = useQuery({
    queryKey: ["erp", "any", entity, filters ?? null],
    queryFn: ({ signal }) => api.get<{ rows: { id: string }[] }>(docPath(entity), { fields: "id", limit: 1, ...(filters ? { filters: JSON.stringify(filters) } : {}) }, signal),
  });
  return !!q.data && q.data.rows.length === 0;
}

/** A desk's empty state when what it needs was never made: what is missing, and where it is made. */
export function SetUp({ title, why, to, label }: { title: string; why: string; to: string; label: string }) {
  return (
    <StatePanel kind="empty" title={isolate(title)} action={<Link className="btn btn-primary btn-md" to={to}>{label}</Link>}>
      {why}
    </StatePanel>
  );
}

/** A record's action, as the generic form calls it; `key` makes it safe to send again. */
export function act<T = unknown>(entity: string, id: string, name: string, body: Record<string, unknown> = {}, key?: string): Promise<T> {
  return api.post<T>(docPath(entity, id, "actions", name), body, key ? { idempotencyKey: key } : undefined);
}

/** A text box's value as the server wants it: nothing when blank. */
export const blank = (v: string | null | undefined): string | undefined => (v && v.trim() ? v.trim() : undefined);

/** A number box that keeps what was typed as text, so a figure is never turned into a float. */
export function amountInput(v: string, set: (v: string) => void, props: { id?: string; label: string; disabled?: boolean }) {
  return (
    <input
      id={props.id}
      className="input num"
      inputMode="decimal"
      autoComplete="off"
      aria-label={props.label}
      disabled={props.disabled}
      value={v}
      onChange={(e) => set(e.target.value.replace(/[^0-9.\-]/g, ""))}
    />
  );
}
