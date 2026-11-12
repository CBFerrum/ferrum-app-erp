// Where each desk answers. A record-bound desk takes over `/e/<Entity>/:id`, the page the sidebar's
// list, the Create menu and every link already open, and keeps the record's own form (every action
// it has) as a second tab at `/f/<Entity>/:id`; a desk without a record takes over the list page of
// the entity whose menu row it belongs to (the row is shown only to people who can read it), and the
// plain list stays at `/l/<Entity>`. A new record of a taken-over entity goes to the plain form.

export interface Desk {
  entity: string;
  /** "record": takes over the record's page; "list": takes over the entity's list page. */
  kind: "record" | "list";
}

export const DESKS: Desk[] = [
  { entity: "PaymentRun", kind: "record" },
  { entity: "TaxReturn", kind: "record" },
  { entity: "CarrierShipment", kind: "record" },
  { entity: "Promotion", kind: "record" },
  { entity: "MrpRun", kind: "record" },
  { entity: "CarrierShipment", kind: "list" },
  { entity: "Promotion", kind: "list" },
  { entity: "MrpRun", kind: "list" },
  { entity: "StoredValueAccount", kind: "list" },
  { entity: "WithholdingCategory", kind: "list" },
  { entity: "NumberSeries", kind: "list" },
];

export const isDesk = (entity: string, kind: Desk["kind"]): boolean => DESKS.some((d) => d.entity === entity && d.kind === kind);

/** The record's page (its desk when it has one) and its plain form. */
export const recordPath = (entity: string, id: string): string => `/e/${entity}/${id}`;
export const formPath = (entity: string, id: string): string => `/f/${entity}/${id}`;
/** The entity's list as the generic page draws it, for a desk that took the usual address over. */
export const listPath = (entity: string): string => `/l/${entity}`;
