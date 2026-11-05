# Finance

The accountant's work beyond posting, on ERP's ledger.

- **Bank accounts** tied to their ledger accounts, statements imported from CSV (importing one again
  skips what is already there), and a reconciliation workbench that pairs exact matches so none is
  used twice.
- **Payment allocation**: a customer's or supplier's payments, credit notes and journal credits
  applied to their open invoices in one document, with the exchange gain or loss of each pairing.
  Advances settle an invoice oldest first.
- **Payment terms** that split an invoice or order into installments with their due dates and early
  payment discounts, and **dunning** that reminds a customer of overdue installments level by level,
  with fees and interest.
- **Budgets** per account for a cost centre or project, phased by month and checked against what the
  books hold and, when the budget asks, what open orders and requests have committed.
- **Exchange revaluation** of foreign currency balances at a period's end, reversed the day after,
  and **period closing**, which moves the year's income and expenses to the closing account and locks
  the year so nothing more posts into it.
- **Subscriptions** billed once per period, prorated where a plan prorates.
- **Reports**: budget variance, the bank reconciliation statement, a cash flow forecast, receivables
  by installment, dunning and subscription figures, and an overview of what needs attention today.
