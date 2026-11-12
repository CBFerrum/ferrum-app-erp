// Stored value (src/stored_value.rs, packages/accounting/entities/stored_value.yaml): the bodies the
// desk sends and the shapes it reads. A card's code and PIN go to one place only, the kind's `prove`
// (and `balance`, which takes them as secret parameters); everything that changes something takes the
// proof that `prove` answers and refuses a code or a PIN. Nothing here keeps a secret: the builders take
// them as arguments and return bodies, and the desk clears its boxes as the call ends.

import { humanize } from "@ferrum/client";
import { blank } from "./desk";

export type Operation = "issue" | "redeem" | "refund" | "transfer" | "transfer_to" | "new_code" | "hold";

export interface Kind {
  id: string;
  kind_name: string;
  company: string;
  holds: "Money" | "Units";
  unit_name?: string | null;
  requires_pin: boolean;
  can_top_up: boolean;
  disabled?: boolean;
  hold_hours?: number | null;
}

export interface Balance {
  account: string;
  balance: string;
  held: string;
  available: string;
  holds: "Money" | "Units";
  state: string;
  expires_on: string | null;
}

export interface Hold {
  hold: string;
  account: string;
  amount: string;
  held: string;
  available: string;
  expires_at: string;
}

export interface Proof {
  proof: string;
  code: string | null;
  expires_in_minutes: number;
}

/** `prove`: the code and PIN the person typed (a blank code makes the server make one) and the effect key a retry repeats. */
export const proveBody = (operation: Operation, code: string, pin: string, key: string) => ({
  operation,
  key,
  ...(blank(code) ? { code: code.trim() } : {}),
  ...(blank(pin) ? { pin: pin.trim() } : {}),
});

/** `balance`: a code is required, the PIN when the kind has one. */
export const balanceBody = (code: string, pin: string) => ({ code: code.trim(), ...(blank(pin) ? { pin: pin.trim() } : {}) });

/** `issue`: the proof of the new card's code, never the code. */
export const issueBody = (o: { amount: string; reason: string; holder: string | null; proof: string }) => ({
  amount: o.amount.trim(),
  reason: o.reason.trim(),
  ...(o.holder ? { holder: o.holder } : {}),
  proof: o.proof,
});

/** `hold` for a submitted sales order, with the proof of the card's code. */
export const holdBody = (o: { order: string; amount: string; proof: string }) => ({ order: o.order, amount: o.amount.trim(), proof: o.proof });

/** An account's `adjust`: positive tops it up. */
export const topUpBody = (o: { amount: string; reason: string }) => ({ amount: o.amount.trim(), reason: o.reason.trim() });

/** What a body must never carry: a code or a PIN in an operation that is logged. */
export const SECRET_KEYS = ["code", "pin", "to_code"] as const;
export const carriesSecret = (body: Record<string, unknown>): boolean => SECRET_KEYS.some((k) => k in body);

/** An entry of the ledger as the report names its type: the ids the server writes, never a label. */
export const entryWord = (id: string, words: Record<string, string>): string => words[id] ?? humanize(id);

/** Whether a hold is still running at `now`, by the time the server gave. */
export const running = (h: Hold, now: number): boolean => Date.parse(h.expires_at) > now;
