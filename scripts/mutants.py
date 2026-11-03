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
LINE = ("-p", "ferrum-wms", "--test", "line_it")
QUALITY = ("-p", "ferrum-quality", "--test", "quality_it")
BOOKS = ("-p", "ferrum-finance", "--test", "books_it")
TASKS = "addons/wms/src/tasks.rs"
WAVES = "addons/wms/src/waves.rs"
RUNS = "addons/mes/src/runs.rs"
QUAL = "addons/quality/src/lib.rs"
# Generous enough to compile and run; passing it means the mutant hung the test.
TIMEOUT = 600

# name, file, text to replace, what breaks it, test
MUTANTS = [
    # Finance books: each book is the common ledger and its own entries, and stays so.
    ("books-reversal-leaves-its-book", "src/ledger.rs", "                finance_book: *book,\n", "                finance_book: None,\n", BOOKS),
    ("books-reports-see-every-book", "packages/accounting/sql/003_finance_books.sql", "  where finance_book_id is null or finance_book_id = nullif(current_setting('ferrum.finance_book', true), '')::uuid;", "  where true;", BOOKS),
    ("books-close-as-one", "addons/finance/src/closing.rs", "            books.entry(book).or_default().push((account, cc, balance));", "            books.entry(None).or_default().push((account, cc, balance));", BOOKS),
    ("books-change-what-parties-owe", "src/accounting.rs", 'if id(doc, "finance_book").is_some() && (s(l, "party_type").is_some() || matches!(typ, "Receivable" | "Payable")) {', "if false {", BOOKS),
    ("wms-release-sends-again", TASKS, 'if matches!(t.state(), "Released" | "Moving" | "Arrived") {\n        let same', 'if false {\n        let same', WMS),
    ("wms-changes-hands-quietly", TASKS, "            if !same {\n", "            if false {\n", WMS),
    ("wms-wrong-tote-accepted", TASKS, "        if scanned(&c.unit_scan) != label {\n", "        if false {\n", WMS),
    ("wms-wrong-item-accepted", TASKS, "if slotting::item_by_scan(tx, ctx.tenant, scanned(&c.item_scan)).await? != Some(t.item) {", "if false {", WMS),
    ("wms-wrong-lot-accepted", TASKS, "        if scanned(got) != expected {\n", "        if false {\n", WMS),
    ("wms-location-alone-proves-it", TASKS, '    if scanned(&c.item_scan).is_empty() {\n        return Err(Error::field("item_scan"', '    if false {\n        return Err(Error::field("item_scan"', WMS),
    ("wms-rejection-rolled-back", "addons/wms/src/site.rs", "        tx.commit().await?;\n        match refused {", "        if let Some(no) = refused.take() {\n            return Err(no.into());\n        }\n        tx.commit().await?;\n        match refused {", WMS),
    ("wms-used-scan-finishes-a-task", TASKS, "if !position::transfer(tx, ctx, t.warehouse, kind, &source, &dest, qty, qty, &cause).await? {", "if !position::transfer(tx, ctx, t.warehouse, kind, &source, &dest, qty, qty, &cause).await? && false {", WMS),
    ("wms-any-scan-repeats", TASKS, "where tenant_id = $1 and task_id = $2 and operation = $3", "where tenant_id = $1 and task_id = $2 and (operation = $3 or true)", WMS),
    ("wms-manual-by-anyone", TASKS, 'if !ctx.is_admin && !ctx.has_role("Warehouse Manager") {', "if false {", WMS),
    ("wms-manual-looks-scanned", TASKS, '(json!({"manual": why, "by": ctx.email}), "Manual")', '(json!({"manual": why, "by": ctx.email}), "Scan")', WMS),
    ("wms-robots-short-load-released", TASKS, "let lost = qty < t.qty && t.robot.is_some();", "let lost = false;", WMS),
    ("wms-lost-counts-what-arrived", TASKS, "    let left = t.qty - done;\n", "    let left = t.qty;\n", WMS),
    ("wms-rejected-after-taken-sticks", TASKS, '(State::Failed | State::Aborted | State::OutcomeUnknown | State::Rejected, "Released" | "Moving" | "Arrived")', '(State::Failed | State::Aborted | State::OutcomeUnknown, "Released" | "Moving" | "Arrived")', WMS),
    ("wms-platform-finishes-a-stations-task", TASKS, "    if t.command.is_some() && !arrived {", "    if t.robot.is_some() && !arrived {", WMS),
    ("wms-held-stock-reaches-the-line", TASKS, '(t.kind == "Pick" || t.production_run.is_some()) && ferrum_quality', 't.kind == "Pick" && ferrum_quality', LINE),
    ("wms-line-stock-claimed-twice", WAVES, "let free = wanted.min((there - claimed).max(Decimal::ZERO));", "let free = wanted.min(there.max(Decimal::ZERO));", LINE),
    ("wms-ready-while-on-its-way", WAVES, "        if have < need {\n            missing = true;\n            continue;\n        }\n", "", LINE),
    ("wms-hold-tells-no-run", "addons/wms/src/holds.rs", "    crate::waves::recheck_supply(tx, ctx, item).await\n", "    Ok(())\n", WMS),
    ("wms-lifted-hold-tells-no-run", "addons/wms/src/holds.rs", "    if let Some(item) = held_item {\n", "    if let Some(item) = held_item.filter(|_| false) {\n", LINE),
    ("wms-finished-run-keeps-the-line", WAVES, "update wms_supply set closed_at = now() where tenant_id = $1 and run_id = $2 and closed_at is null", "update wms_supply set closed_at = null where tenant_id = $1 and run_id = $2 and closed_at is null", LINE),
    ("mes-machine-starts-on-release", RUNS, 'if s(&doc, "material_source") == Some("Preloaded") {\n        audit::record', 'if true {\n        audit::record', MES),
    ("mes-started-by-hand-without-material", RUNS, 'if !matches!(s(&doc, "material_status"), Some("At Station" | "Preloaded")) {', "if false {", MES),
    ("mes-reservation-ignored", RUNS, "Ok(have.unwrap_or_default() - held - others)", "Ok(have.unwrap_or_default() - held - others * Decimal::ZERO)", MES),
    ("mes-exposed-start-allowed", RUNS, "    if !allowed.engine_only {\n", "    if false {\n", MES),
    ("mes-start-does-not-ask-again", RUNS, "    let machine = gate(k, ctx, tx, &doc).await?;\n    let needs = reserved(tx, ctx.tenant, run).await?;\n", "    let machine = gate(k, ctx, tx, &doc).await.unwrap_or(None).or(Some((Uuid::nil(), Value::Null))).filter(|_| false);\n    let needs = reserved(tx, ctx.tenant, run).await?;\n", MES),
    ("mes-hold-leaves-the-start", "addons/mes/src/lib.rs", '    h.register("QualityHold", Arc::new(HoldHook));\n', "", MES),
    ("mes-preloaded-unrecorded", RUNS, 'audit::record(tx, ctx, "ProductionRun", run, "mes.preloaded", None, json!({"by": ctx.email})).await?;\n', "", MES),
    ("mes-finished-start-sent-again", RUNS, '    match s(&doc, "command").and_then(|c| c.parse().ok()) {\n        Some(command) => Ok(Some(command)),\n        None => authorize(k, ctx, tx, run).await,\n    }\n', "    authorize(k, ctx, tx, run).await\n", LINE),
    ("wms-ready-with-a-material-nobody-brings", WAVES, "        let Some(line) = line else {\n            by_hand = true;\n            continue;\n        };\n", "        let Some(line) = line else { continue };\n", WMS),
    ("wms-reversal-looks-at-the-line", "addons/wms/src/posting.rs", "        } else if m.reversal_of.is_some() {\n", "        } else if false {\n", WMS),
    ('wms-recovery-forgets-the-order', 'addons/wms/src/tasks.rs', '    if owed && !arrived && !qty.is_zero() {', '    if false && owed && !arrived && !qty.is_zero() {', WMS),
    ('wms-found-on-the-way-counts-as-arrived', 'addons/wms/src/tasks.rs', 'let arrived = moved && (!owed || t.to_bin == Some(bin.id));', 'let arrived = moved;', WMS),
    ('wms-found-short-unrecorded', 'addons/wms/src/tasks.rs', '    crate::waves::fell_short(k, ctx, tx, &t, left - qty).await?;\n', '', WMS),
    ('wms-given-up-pick-unrecorded', 'addons/wms/src/tasks.rs', '            crate::waves::fell_short(k, ctx, tx, &t, t.qty).await?;\n            finished(k, ctx, tx, &t).await?;\n            Ok("Cancelled".into())', '            finished(k, ctx, tx, &t).await?;\n            Ok("Cancelled".into())', WMS),
    ('wms-shipment-waits-for-nothing', 'addons/wms/src/waves.rs', '        for shipment in empty {', '        for shipment in empty.into_iter().take(0) {', WMS),
    ("wms-found-at-the-line-unknown-to-the-run", "addons/wms/src/tasks.rs", "    if let (Some(run), true) = (run, arrived) {\n        crate::waves::arrived_at_line", "    if let (Some(run), true) = (run, false) {\n        crate::waves::arrived_at_line", LINE),
    ("wms-recovery-leaves-the-line-short", "addons/wms/src/tasks.rs", "    let owed = t.wave.is_some() || run.is_some();", "    let owed = t.wave.is_some();", LINE),
    ("wms-landed-moves-replayed", "addons/wms/src/posting.rs", " \\\n         and not exists (select 1 from wms_move w where w.tenant_id = m.tenant_id and w.operation = 'stock:' || m.id) order by m.seq", " order by m.seq", WMS),
    ("wms-two-balances", "addons/wms/packages/wms/sql/001_wms.sql", "  if on_books <> in_place then", "  if false then", WMS),
    ("wms-issues-what-nobody-picked", "addons/wms/src/posting.rs", "    if waiting < qty {", "    if false {", WMS),
    ("wms-arriving-is-delivering", TASKS, 'json!({"status": "Arrived", "custody": "In Transit", "reason": "Arrived: confirm the unit at its destination"})', 'json!({"status": "Done", "custody": "In Place"})', WMS),
    ("wms-parcel-goes-twice", "addons/wms/src/shipping.rs", 'd(line, "verified_qty") - d(line, "handed_over_qty")', 'd(line, "verified_qty")', WMS),
    ("wms-receives-twice", "addons/wms/src/inbound.rs", 'if !matches!(status, "Expected" | "Receiving") {\n        return Err(Error::invalid("this shipment is already received"));', 'if status == "Received" {\n        return Err(Error::invalid("this shipment is already received"));', WMS),
    ("wms-scan-counts-twice", "addons/wms/src/inbound.rs", '    if !first {\n        return Ok(json!({"repeated": true}));\n    }\n', "    let _ = first;\n", WMS),
    ("wms-part-of-a-code-matches", "addons/wms/src/slotting.rs", "upper(item_code) = upper($2)", "upper(item_code) like '%' || upper($2) || '%'", WMS),
    ("wms-held-stock-allocated", WAVES, "and p.status = 'Available' and p.qty > p.allocated \\\n", "and p.qty > p.allocated \\\n", WMS),
    ("wms-run-supplied-twice", WAVES, ".rows_affected()\n        == 0\n    {\n        return Ok(());", ".rows_affected()\n        == 9\n    {\n        return Ok(());", WMS),
    ("wms-short-supply-silent", WAVES, "    if !short.is_empty() {\n", "    if false {\n", WMS),
    ("mes-unapproved-revision-runs", RUNS, 'if s(&rev, "status") != Some("Approved") {', "if false {", MES),
    ("mes-wrong-lot-sticks", RUNS, "            if *qty < Decimal::ZERO {\n                // A wrong lot", "            if false {\n                // A wrong lot", MES),
    ("mes-take-back-unbounded", RUNS, "                if -*qty > left {", "                if false {", MES),
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
