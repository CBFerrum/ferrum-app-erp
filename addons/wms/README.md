# Warehouse Management

Runs a warehouse down to the bin, on ERP's stock.

- **One balance.** Positions by bin, lot, owner and status are a subledger of ERP's stock. The database
  refuses any commit where they differ, and every position change has its ledger row.
- **Receiving** by scan against what was expected: damaged goods go to quarantine, over-receipt needs a
  manager, unknown goods stay off the books until someone says what they are, and a difference is
  settled by a person.
- **Putaway, picking and replenishment** with bin rules (weight, volume, hazard, temperature) and
  FEFO or FIFO allocation. A scan that does not match is refused.
- **Packing and shipping**: each unit verified, the label and weight checked. A partial shipment says
  it is one, and each parcel carries only what has not gone.
- **Line supply**: releasing a production run brings its material to the production bin.
- **Counts** that can be blind, with a second counter and someone else to approve a difference.
- **Returns, holds, and storage billing** for customers' stock, kept off this company's books.
- **Robots**, through Operations. Arriving is not delivering: stock moves on a scan at the destination.

Not built: zone, cluster and goods-to-person picking, substitutions, cartonization, carrier connectors.
Automation coordinates here and controls nothing: a robot's route, traffic and safety are its fleet
manager's. Tried on a simulated fleet only.
