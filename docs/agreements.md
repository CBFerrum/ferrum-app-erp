# Purchase and sales agreements

An agreement (a blanket order) records what was agreed with a supplier (**Purchase Agreement**, Buying) or a
customer (**Sales Agreement**, Selling): items, an agreed quantity and price for each, and the dates it holds.

## How orders use it
- An order line names the agreement in **Agreement**. The line takes the agreed price (per the item's stock unit,
  times the line's unit factor: a box of ten costs ten times the agreed price) and counts against the agreed
  quantity in stock units. The price list and pricing rules do not apply to such a line.
- The order must be of the agreement's company and party, in its currency, and dated within its dates; the
  agreement must be submitted and not closed; it must have a line for the item.
- **Create Purchase Order** / **Create Sales Order** on the agreement makes a draft order for what is left of
  each line.

## What is ordered
Each line's **Ordered** is counted from the submitted orders that name it, less what they were closed short of,
each time one of them is submitted, cancelled, changed (Update Items), closed or re-opened. The count is taken
under the agreement's row lock in the order's own transaction, so two orders submitted at once cannot together
go past what was agreed: the one that would is refused, saying by how much. **Ordered (%)** is the share of the
agreed quantities ordered; the status is **Open**, **Fulfilled** when every line is ordered in full, or
**Closed** (Close stops further orders; Re-open lets them draw again).

An agreement that a submitted order draws on cannot be cancelled: cancel or change the orders first.

## Not covered
Agreements by value only, price breaks by quantity, choosing an agreement for a line automatically, release
schedules, and agreements on requests for quotation.
