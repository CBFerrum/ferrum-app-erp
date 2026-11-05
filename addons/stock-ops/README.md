# Stock Operations

The warehouse's everyday documents, through ERP's inventory engine and ledger.

- **Stock reconciliation**: counted quantities (and valuation rates, when given) post their
  differences, and cancelling reverses them exactly.
- **Landed costs**: freight, duty and other charges added to the cost of received stock still on hand.
- **Pick lists** by warehouse, batch (earliest expiry first) and serial, holding what they picked
  until it is delivered, and **packing slips** for a delivery note, which then leaves only fully packed.
- **Quality inspection**: templates of parameters with acceptance criteria, and receipts or deliveries
  that wait for an accepted inspection when their items need one.
- **Item variants** made from a template's attributes, which keep following the template where they
  were not changed, and **product bundles** sold as kits whose components move with the delivery.
- **Reports**: reconciliation variance, landed cost allocation, pick list status, inspections, the
  variant stock matrix and stock ageing.
