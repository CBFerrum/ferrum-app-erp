// What is running where, and what is ready to go. A stopped run is at the top: that is where someone
// is needed.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { LayoutDashboardIcon } from "lucide-react";
import { Link } from "react-router";
import { formatDecimal, refusal } from "@ferrum/client";
import { Badge, Button, LoadFailed, PageLoading, StatePanel, toast } from "@ferrum/kit";
import { post, type Run, rows } from "./data";
import { runWords } from "./logic";
import { Screen } from "./Tabs";

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
          {r.number} · {r._titles?.work_order ?? ""} · {r.operator_email}
        </div>
        {r.note ? <div className="muted">{r.note}</div> : null}
      </Link>
      <span className="num mes-row-qty">
        {formatDecimal(r.good_qty ?? 0)} of {formatDecimal(r.qty)}
      </span>
      {r.status === "Planned" ? (
        <Button variant="primary" loading={release.isPending} onClick={() => release.mutate()}>
          Release
        </Button>
      ) : null}
    </li>
  );
}

export function Home() {
  const q = useQuery({ queryKey: ["mes", "runs"], queryFn: () => rows<Run>("ProductionRun", [["status", "in", ORDER]], { limit: 200 }), refetchInterval: 15_000 });
  const list = [...(q.data ?? [])].sort((a, b) => ORDER.indexOf(a.status) - ORDER.indexOf(b.status));
  return (
    <Screen
      section="floor"
      actions={
        list.length ? (
          <Link className="btn btn-primary btn-md" to="/e/ProductionRun/new">
            Plan a run
          </Link>
        ) : undefined
      }
    >
      {q.isPending ? (
        <PageLoading />
      ) : q.isError ? (
        <LoadFailed error={q.error} onRetry={() => void q.refetch()} />
      ) : list.length === 0 ? (
        <StatePanel kind="empty" icon={LayoutDashboardIcon} title="Nothing on the floor" action={<Link className="btn btn-primary btn-md" to="/e/ProductionRun/new">Plan a run</Link>}>
          Runs that are planned, released, running or stopped are listed here.
        </StatePanel>
      ) : (
        <ul className="card mes-list">
          {list.map((r) => (
            <Row key={r.id} r={r} />
          ))}
        </ul>
      )}
    </Screen>
  );
}
