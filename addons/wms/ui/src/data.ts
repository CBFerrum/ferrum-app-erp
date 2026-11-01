// The warehouse's endpoints, and the record lists the screens read.

import { api, docPath } from "@ferrum/client";
import { useQuery } from "@tanstack/react-query";
import { useState } from "react";
import type { Task } from "./logic";

const base = "/api/app/wms";

export const post = <T,>(path: string, body: unknown = {}) => api.post<T>(`${base}${path}`, body);
export const get = <T,>(path: string, query?: Record<string, string>) => api.get<T>(`${base}${path}`, query);

export function rows<T>(entity: string, filters: unknown[], extra: Record<string, string | number> = {}) {
  return api.get<{ rows: T[] }>(docPath(entity), { filters: JSON.stringify(filters), titles: true, ...extra }).then((r) => r.rows);
}

export interface Managed {
  id: string;
  warehouse: string;
  name_of_warehouse: string | null;
  status: string;
}

export interface Overview {
  tasks: Record<string, number>;
  stock: Record<string, string>;
  blocked_bins: number;
  closed_zones: number;
}

const KEY = "ferrum.wms.warehouse";

function remembered(): string | null {
  try {
    return localStorage.getItem(KEY);
  } catch {
    return null;
  }
}

/** Which warehouse this person is in. Remembered on this device only: a handheld stays in its
 *  building. */
export function useWarehouse() {
  const q = useQuery({ queryKey: ["wms", "warehouses"], queryFn: () => rows<Managed>("ManagedWarehouse", [["status", "=", "Managed"]]) });
  const [chosen, setChosen] = useState(remembered);
  const list = q.data ?? [];
  const current = list.find((w) => w.warehouse === chosen) ?? list[0] ?? null;
  const choose = (w: string) => {
    setChosen(w);
    try {
      localStorage.setItem(KEY, w);
    } catch {
      // A private window: it stays chosen until the page closes.
    }
  };
  return { list, current, choose, query: q };
}

export const tasks = (warehouse: string) => get<{ tasks: Task[] }>("/tasks", { warehouse }).then((r) => r.tasks);
