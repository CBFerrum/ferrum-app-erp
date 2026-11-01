# Manufacturing Execution

Runs ERP's work orders on the floor.

- **Release gates**: an approved revision of how it is made, a qualified operator, equipment that can
  run, and material there, or the run says exactly which is missing.
- **A terminal for the operator**, with big buttons. Every press has its own id, so a shaky connection
  counts once. With the link down, a site's console takes reports by the run's number.
- **Genealogy**: what went into a lot and what a lot went into, forward for a recall, back for an
  investigation, scrap included.
- **OEE** by a written definition, per workstation and period. A figure with nothing to divide by is
  unknown, and time nobody saw is never counted as running.
- **Approved revisions** that do not change once used. Whoever wrote one does not approve it.

It dispatches a reference to an approved job and never writes controller logic. Discrete manufacturing
only: batch, continuous and energy reporting are partner work. Tried on a simulated press only.
