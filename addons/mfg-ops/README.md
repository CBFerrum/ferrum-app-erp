# Manufacturing Operations

How ERP's work orders are made: where, in what order, for how long and at what cost.

- **Workstations** with an hour rate, working hours and a capacity of parallel jobs, and **operations**
  and **routings** that cost a BOM's operating time at those rates, so it reaches finished goods.
- **Work orders** copy their operations and are scheduled forward over the workstations' working
  hours when submitted. **Job cards** log the time at each operation, never more jobs at a workstation
  than it holds, and no operation completes more than the one before it.
- **Production plans** from open sales orders, material requests or typed in: a multi-level explosion
  netted against projected stock, then work orders and material requests (purchase order drafts
  without Procurement).
- **Subcontracting**: a supplier makes goods from materials sent to a warehouse at their premises, and
  the receipt values the goods at the materials, the service and the additional costs.
- **Reports**: job card time, cost and efficiency, workstation load against capacity, production plan
  and operation progress, and materials and goods per subcontracting order.
