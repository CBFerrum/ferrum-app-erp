# Manufacturing Execution

Runs ERP's work orders on the floor.

- **Release gates**: an approved revision of how it is made, a qualified operator, equipment that can
  run, and material there and not set aside for another run, or the run says exactly which is missing.
- **A machine starts on a second gate**, once the material is at the station: a warehouse says so by
  delivering it, or a supervisor does, on the record. Everything is asked again at that moment, a hold
  on the way stops the start, and a machine already loaded has to say so on the run. Its start
  operation must be kept to this app, so nothing can ask for it around these gates.
- **A terminal for the operator**, with big buttons. Every press has its own id, so a shaky connection
  counts once. With the link down, a site's console takes reports by the run's number.
- **Genealogy**: what went into a lot and what a lot went into, forward for a recall, back for an
  investigation, scrap included.
- **OEE** by a written definition, per workstation and period. A figure with nothing to divide by is
  unknown, and time nobody saw is never counted as running.
- **Approved revisions** that do not change once used. Whoever wrote one does not approve it.

It dispatches a reference to an approved job and never writes controller logic. Discrete manufacturing
only: batch, continuous and energy reporting are partner work. Tried on a simulated press only.
