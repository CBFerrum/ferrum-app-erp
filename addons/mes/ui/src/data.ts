// The shop floor's endpoints and the records its screens read.

import { api, docPath } from "@ferrum/client";

const base = "/api/app/mes";

export const post = <T,>(path: string, body: unknown = {}) => api.post<T>(`${base}${path}`, body);
export const get = <T,>(path: string, query?: Record<string, string>) => api.get<T>(`${base}${path}`, query);

export function rows<T>(entity: string, filters: unknown[], extra: Record<string, string | number> = {}) {
  return api.get<{ rows: T[] }>(docPath(entity), { filters: JSON.stringify(filters), titles: true, ...extra }).then((r) => r.rows);
}

export interface Run {
  id: string;
  reference?: string | null;
  work_order: string;
  workstation: string;
  revision: string;
  operation: string;
  operator_email: string;
  qty: string;
  status: string;
  good_qty?: string | null;
  scrap_qty?: string | null;
  rework_qty?: string | null;
  note?: string | null;
  version?: number;
  _titles?: Record<string, string>;
}

export interface OeeAnswer {
  input: { planned_secs: number; run_secs: number; unknown_secs: number; total_count: number; good_count: number };
  figures: { availability: number | null; performance: number | null; quality: number | null; oee: number | null; coverage: number | null; version: number };
}

export interface TraceStep {
  item: string;
  lot: string;
  into_item: string;
  into_lot: string;
  qty: string;
  depth: number;
  scrapped: boolean;
}
