#!/usr/bin/env python3
"""Deliberately breaks invariants and checks that the tests notice, as Ferrum's own script does.

Each mutant removes one guarantee of the warehouse, the shop floor or quality; its test must then
fail. One that survives means that test proves nothing, and this script fails. Needs the test
databases, as `cargo test` does. It restores every file it touches, even when interrupted, and
refuses to start on a dirty file.

    scripts/mutants.py            # all of them
    scripts/mutants.py wms        # those whose name contains `wms`
"""

import os
import signal
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WMS = ("-p", "ferrum-wms", "--test", "wms_it")
MES = ("-p", "ferrum-mes", "--test", "mes_it")
QUALITY = ("-p", "ferrum-quality", "--test", "quality_it")
TASKS = "addons/wms/src/tasks.rs"
WAVES = "addons/wms/src/waves.rs"
RUNS = "addons/mes/src/runs.rs"
QUAL = "addons/quality/src/lib.rs"
# Generous enough to compile and run; passing it means the mutant hung the test.
TIMEOUT = 600

# name, file, text to replace, what breaks it, test
MUTANTS = [
    ("wms-two-balances", "addons/wms/packages/wms/sql/001_wms.sql", "  if on_books <> in_place then", "  if false then", WMS),
    ("wms-issues-what-nobody-picked", "addons/wms/src/posting.rs", "    if waiting < qty {", "    if false {", WMS),
    ("wms-arriving-is-delivering", TASKS, 'json!({"status": "Arrived", "custody": "In Transit", "reason": "Arrived: confirm the unit at its destination"})', 'json!({"status": "Done", "custody": "In Place"})', WMS),
    ("wms-parcel-goes-twice", "addons/wms/src/shipping.rs", 'd(line, "verified_qty") - d(line, "handed_over_qty")', 'd(line, "verified_qty")', WMS),
    ("wms-receives-twice", "addons/wms/src/inbound.rs", 'if !matches!(status, "Expected" | "Receiving") {\n        return Err(Error::invalid("this shipment is already received"));', 'if status == "Received" {\n        return Err(Error::invalid("this shipment is already received"));', WMS),
    ("wms-scan-counts-twice", "addons/wms/src/inbound.rs", '    if !first {\n        return Ok(json!({"repeated": true}));\n    }\n', "    let _ = first;\n", WMS),
    ("wms-part-of-a-code-matches", "addons/wms/src/slotting.rs", "upper(item_code) = upper($2)", "upper(item_code) like '%' || upper($2) || '%'", WMS),
    ("wms-held-stock-allocated", WAVES, "and p.status = 'Available' and p.qty > p.allocated \\\n", "and p.qty > p.allocated \\\n", WMS),
    ("wms-run-supplied-twice", WAVES, ".rows_affected() == 0 {\n        return Ok(());", ".rows_affected() == 9 {\n        return Ok(());", WMS),
    ("wms-short-supply-silent", WAVES, "    if !short.is_empty() {\n", "    if false {\n", WMS),
    ("mes-unapproved-revision-runs", RUNS, 'if s(&rev, "status") != Some("Approved") {', "if false {", MES),
    ("mes-unqualified-operator-runs", RUNS, "    if qualified.is_none() {", "    if false {", MES),
    ("mes-short-material-released", RUNS, "    if !missing.is_empty() {", "    if false {", MES),
    ("mes-author-approves", RUNS, 'if s(&doc, "created_by") == Some(ctx.user.to_string().as_str()) && !ctx.is_admin {', "if false {", MES),
    ("mes-event-counts-twice", RUNS, '    if seen.is_some() {\n        return Ok(json!({"repeated": true}));\n    }\n', "    let _ = seen;\n", MES),
    ("quality-placer-releases", QUAL, 'if s(&doc, "placed_by") == Some(ctx.email.as_str()) {', "if false {", QUALITY),
    ("quality-held-stock-ships", QUAL, "if let Some((_, rework)) = hold_on(hc.tx, hc.ctx.tenant, item, lot).await? {", "if let Some((_, rework)) = hold_on(hc.tx, hc.ctx.tenant, item, lot).await?.filter(|_| false) {", QUALITY),
]


def dirty(path: str) -> bool:
    return bool(subprocess.run(["git", "status", "--porcelain", "--", path], cwd=ROOT, capture_output=True, text=True).stdout.strip())


def main() -> int:
    only = sys.argv[1] if len(sys.argv) > 1 else ""
    chosen = [m for m in MUTANTS if only in m[0]]
    for name, path, old, _, _ in chosen:
        if old not in (ROOT / path).read_text():
            print(f"{name}: its text is gone from {path}; update this script", file=sys.stderr)
            return 2
        if dirty(path):
            print(f"{path} has uncommitted changes; commit them first", file=sys.stderr)
            return 2
    env = {**os.environ, "CARGO_INCREMENTAL": "0"}
    survived = []
    for name, path, old, new, test in chosen:
        file = ROOT / path
        original = file.read_text()
        try:
            file.write_text(original.replace(old, new, 1))
            # Its own process group: a hung test binary must go with cargo, not spin on as an orphan.
            job = subprocess.Popen(["cargo", "test", *test], cwd=ROOT, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, start_new_session=True)
            try:
                output, _ = job.communicate(timeout=TIMEOUT)
            except subprocess.TimeoutExpired:
                os.killpg(job.pid, signal.SIGKILL)
                job.wait()
                print(f"caught   {name} (it hung)", flush=True)
                continue
            if "error[E" in output or "could not compile" in output:
                print(f"BROKEN   {name}: the mutant does not compile\n{output[-1500:]}")
                survived.append(name)
            elif job.returncode == 0:
                print(f"SURVIVED {name}: the tests stayed green", flush=True)
                survived.append(name)
            else:
                print(f"caught   {name}", flush=True)
        finally:
            file.write_text(original)
    print(f"{len(chosen) - len(survived)} of {len(chosen)} caught")
    return 1 if survived else 0


if __name__ == "__main__":
    sys.exit(main())
