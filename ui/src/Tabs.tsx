// The accountant's screens, at the top of each of them. A screen shows only for people who can read
// what it works on, and only where the add-on that serves it is installed.

import { Link, useLocation } from "react-router";
import { can, useMetaIndex } from "@ferrum/records";

interface Tab {
  to: string;
  label: string;
  /** Entity the person must be able to read for the screen to be of use. */
  needs: string;
  /** Other paths that count as this tab. */
  also?: string[];
}

export const STATEMENTS = ["/r/profit_and_loss", "/r/balance_sheet", "/r/cash_flow"];

const TABS: Tab[] = [
  { to: "/r/finance_overview", label: "Today", needs: "BankTransaction" },
  { to: "/r/bank_reconciliation", label: "Bank", needs: "BankAccount" },
  { to: "/e/PaymentAllocation/new", label: "Allocate payments", needs: "PaymentAllocation" },
  { to: "/r/profit_and_loss", label: "Statements", needs: "JournalEntry", also: STATEMENTS },
  { to: "/e/ExchangeRevaluation/new", label: "Revalue", needs: "ExchangeRevaluation" },
];

export function AccountsTabs() {
  const { pathname } = useLocation();
  const meta = useMetaIndex().data;
  const shown = TABS.filter((t) => meta?.entities.some((e) => e.name === t.needs && can(e.permissions, "read")));
  if (shown.length < 2) return null;
  return (
    <nav className="erp-tabs" aria-label="Accounts">
      {shown.map((t) => (
        <Link key={t.to} to={t.to} aria-current={[t.to, ...(t.also ?? [])].includes(pathname) ? "page" : undefined}>
          {t.label}
        </Link>
      ))}
    </nav>
  );
}
