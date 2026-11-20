# Drop shipment

A sales order line marked **Supplier Delivers**, with its **Delivering Supplier**, is sent by the supplier straight
to the customer. The company never holds the goods.

## The flow
1. **Sales order.** The line needs an item and a supplier. It holds no stock: it is left out of reservations and
   promises, of requirements runs (both kinds), production plans, reorder levels and pick waves.
2. **Order from Supplier** on the submitted sales order makes a draft purchase order per supplier for the drop-ship
   lines not yet ordered, shipped to the sales order's ship-to address (else the customer's address) and naming the
   customer. Prices are the buying side's own. Each purchase order line says which sales order line it is for.
3. **Purchase order.** Submitting, cancelling, changing (Update Items), closing or re-opening it recounts each sales
   line's **Ordered from Supplier** under the sales order's row lock; more than the sales line's quantity is refused,
   so two purchase orders cannot together order a line twice.
4. **The supplier ships: the purchase receipt.** Made from the purchase order as usual. A drop-ship line needs no
   warehouse, rejects nothing and moves no stock. It books the line's net amount in company currency:
   - debit **Cost of Goods Sold**, credit **Stock Received But Not Billed**;
   - and marks the sales order line delivered.
   Cancelling the receipt reverses both. A purchase return against it books the reverse and takes the delivery back.
5. **The supplier's bill.** From the receipt: "received but not billed" is cleared at the receipt's value for the
   quantity billed, and any difference in price goes to cost of sales. From the order without a receipt: debited
   to "received but not billed" as for any goods. With Update Stock: straight to cost of sales, no stock.
6. **The customer's invoice.** From the sales order as always: revenue and the receivable. With Update Stock its
   drop-ship lines move no stock, book no cost (the receipt did) and are not counted delivered a second time.

A delivery note may not deliver a drop-ship line; Create Delivery Note leaves them out, and refuses when only
drop-ship lines are open.

## Not covered
Shipping notices from the supplier's portal, tracking the supplier's parcel, landed cost on drop-ship lines,
inter-company drop shipment, back-to-back orders that pass through stock, and lowering a sales line below what is
already ordered from the supplier (the purchase order is changed first).
