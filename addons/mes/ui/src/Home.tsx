// What is running where, and what is ready to go. A stopped run is at the top: that is where someone
// is needed.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, NavLink } from "react-router";
import { formatDecimal, refusal } from "@ferrum/client";
import { Badge, Button, EmptyState, ErrorState, PageLoading, toast, useShellTitle } from "@ferrum/kit";
import { post, type Run, rows } from "./data";
import { runWords } from "./logic";

export function Tabs() {
  return (
    <nav className="mes-tabs" aria-label="Shop floor">
      <NavLink to="/" end>
        Floor
      </NavLink>
      <NavLink to="/oee">OEE</NavLink>
      <NavLink to="/trace">Trace a lot</NavLink>
    </nav>
  );
}

const ORDER = ["Paused", "Running", "Released", "Planned"];

function Row({ r }: { r: Run }) {
  const qc = useQueryClient();
  const word = runWords(r.status, r.material_status ?? null);
  const release = useMutation({
    mutationFn: () => post(`/runs/${r.id}/release`),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ["mes"] }),
    onError: (e) => toast(refusal(e), "error"),
  });
  return (
    <li className="mes-row">
      <Link to={`/run/${r.id}`} className="mes-row-main">
        <strong>{r._titles?.workstation ?? "Workstation"}</strong> <Badge tone={word.tone}>{word.label}</Badge>
        <div className="faint">
          {r.number} · {r._titles?.work_order ?? ""} · {formatDecimal(r.good_qty ?? 0)} of {formatDecimal(r.qty)} · {r.operator_email}
        </div>
        {r.note ? <div className="muted">{r.note}</div> : null}
      </Link>
      {r.status === "Planned" ? (
        <Button size="sm" variant="primary" loading={release.isPending} onClick={() => release.mutate()}>
          Release
        </Button>
      ) : null}
    </li>
  );
}

export function Home() {
  useShellTitle("Shop floor");
  const q = useQuery({ queryKey: ["mes", "runs"], queryFn: () => rows<Run>("ProductionRun", [["status", "in", ORDER]], { limit: 200 }), refetchInterval: 15_000 });
  const list = [...(q.data ?? [])].sort((a, b) => ORDER.indexOf(a.status) - ORDER.indexOf(b.status));
  return (
    <div className="mes-page">
      <Tabs />
      {q.isPending ? (
        <PageLoading />
      ) : q.isError ? (
        <ErrorState error={q.error} onRetry={() => void q.refetch()} />
      ) : list.length === 0 ? (
        <EmptyState title="Nothing on the floor" icon="dashboard">
          <Link to="/e/ProductionRun/new">Plan a run</Link>
        </EmptyState>
      ) : (
        <ul className="card mes-list">
          {list.map((r) => (
            <Row key={r.id} r={r} />
          ))}
        </ul>
      )}
    </div>
  );
}
