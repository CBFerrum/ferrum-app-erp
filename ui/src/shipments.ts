// Shipping (src/shipping.rs, packages/inventory/entities/shipping.yaml): what a desk may offer for a
// shipment in its state (the same conditions the actions carry), and the shapes of the answers. Pure.
// Nothing here names a carrier: rates, services and tracking words are the carrier's, as the server
// read them from the calls a connection describes.

export type Status = "Draft" | "Rated" | "Label Bought" | "In Transit" | "Delivered" | "Exception" | "Cancelled";

export interface Rate {
  service: string;
  amount: string;
  currency?: string | null;
  days?: number | null;
  id?: string | null;
}

export interface Parcel {
  id?: string;
  reference?: string | null;
  weight: string;
  length?: string | null;
  width?: string | null;
  height?: string | null;
  contents?: string | null;
}

export interface Shipment {
  id: string;
  version: number;
  number?: string | null;
  company: string;
  carrier: string;
  reference?: string | null;
  ship_to?: string | null;
  packages: Parcel[];
  status: Status;
  rates?: Rate[] | null;
  service?: string | null;
  rate_amount?: string | null;
  rate_currency?: string | null;
  transit_days?: number | null;
  tracking_number?: string | null;
  tracking_url?: string | null;
  label_file?: string | null;
  cost?: string | null;
  tracking_status?: string | null;
  last_tracked_at?: string | null;
  delivered_at?: string | null;
}

/** What the desk offers, by the conditions of the actions on the entity. */
export const may = {
  rate: (s: Shipment) => !s.tracking_number && s.status !== "Cancelled",
  choose: (s: Shipment) => s.status === "Rated" && (s.rates?.length ?? 0) > 0,
  buy: (s: Shipment) => s.status === "Rated" && !!s.service,
  track: (s: Shipment) => !!s.tracking_number && s.status !== "Cancelled",
  cancel: (s: Shipment) => !!s.tracking_number && s.status !== "Cancelled" && s.status !== "Delivered",
};

export const TONE: Record<Status, "neutral" | "blue" | "green" | "amber" | "red"> = {
  Draft: "neutral",
  Rated: "blue",
  "Label Bought": "blue",
  "In Transit": "amber",
  Delivered: "green",
  Exception: "red",
  Cancelled: "neutral",
};

/** The cheapest rate offered, for a hint; never chosen for the person. */
export const cheapest = (rates: Rate[]): Rate | null => rates.reduce<Rate | null>((best, r) => (best === null || Number(r.amount) < Number(best.amount) ? r : best), null);

/** A connection of the platform as Administration lists it, and what its sign-in is called. */
export interface PlatformConnection {
  id: string;
  name: string;
  enabled: boolean;
  last_error?: string | null;
  health?: { authentication?: string; dead_letters?: number } | null;
}

/** The platform connection a carrier names: by its id, or by its name where the field holds that. */
export const connectionOf = (list: PlatformConnection[] | undefined, field: string | null | undefined): PlatformConnection | null => (field ? (list?.find((c) => c.id === field || c.name === field) ?? null) : null);

/** The body of Choose Rate and Buy Label. */
export const chooseBody = (service: string) => ({ service });
