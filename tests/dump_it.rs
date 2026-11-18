//! The books through a dump and a restore made as the server's own role, as Ferrum's `deploy/dump.sh` makes them (pg_dump
//! with `--enable-row-security` as the system, which reads each partition by its own name; pg_restore into an empty
//! database): from `FERRUM_ERP_DUMP_TEST_DATABASE_URL` into `FERRUM_ERP_RESTORE_TEST_DATABASE_URL` (both reset first),
//! with pg_dump and pg_restore from `PG_BIN` or the PATH. Every partitioned table (the journal's lines, by year) comes
//! back with every row: a year that did not carry the journal's policies read as empty there, and the dump held none of
//! its lines without a word.

mod common;
use common::*;
use ferrum_kernel::{Db, DbConfig};
use serde_json::json;
use std::collections::BTreeMap;
use std::process::Command;

fn url(env: &str, database: &str) -> String {
    std::env::var(env).unwrap_or_else(|_| format!("postgres://ferrum:ferrum@localhost:5432/{database}"))
}

fn bin(name: &str) -> String {
    std::env::var("PG_BIN").map(|d| format!("{d}/{name}")).unwrap_or_else(|_| name.to_string())
}

/// Every partitioned table, with its rows and each partition's, as the system reads them.
async fn partitioned(url: &str) -> BTreeMap<String, i64> {
    let db = Db::connect(DbConfig { url: url.to_string(), max_connections: 1, ..Default::default() }).await.expect("connect");
    let mut tx = db.system_tx().await.unwrap();
    let tables: Vec<String> =
        sqlx::query_scalar("select oid::regclass::text from pg_class where relkind in ('r', 'p') and (relkind = 'p' or relispartition) order by 1")
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    let mut out = BTreeMap::new();
    for t in tables {
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("select count(*) from {t}"))).fetch_one(&mut *tx).await.unwrap();
        out.insert(t, n);
    }
    tx.commit().await.unwrap();
    db.pool.close().await;
    out
}

#[test]
fn the_books_survive_a_dump() {
    run(scenario());
}

async fn scenario() {
    let (live, copy) =
        (url("FERRUM_ERP_DUMP_TEST_DATABASE_URL", "ferrum_erp_dump_test"), url("FERRUM_ERP_RESTORE_TEST_DATABASE_URL", "ferrum_erp_restore_test"));
    let t = T::boot(live.clone(), |_, _| {}).await;
    // An invoice posted today: its journal lines go into this year's partition.
    let c = t.company.clone();
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let advice = uid(&t
        .insert("Item", json!({"item_code": "Advice", "item_name": "Advice", "item_group": products["id"], "stock_uom": nos["id"], "standard_rate": "1000", "is_stock_item": false}))
        .await)
    .to_string();
    let globex = uid(&t.insert("Customer", json!({"customer_name": "Globex"})).await).to_string();
    t.post("SalesInvoice", json!({"company": c, "customer": globex, "items": [{"item": advice, "qty": "1"}]})).await;
    t.k.db.pool.close().await;

    let before = partitioned(&live).await;
    println!("partitioned tables and their partitions: {before:?}");
    assert!(before["journal_line"] > 0, "{before:?}");

    let target = Db::connect(DbConfig { url: copy.clone(), max_connections: 1, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists vault cascade; drop schema if exists public cascade; create schema public;").execute(&target.pool).await.unwrap();
    target.pool.close().await;
    let dir = std::env::temp_dir().join(format!("ferrum-erp-dump-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (file, list) = (dir.join("erp.dump"), dir.join("erp.list"));
    let dumped = Command::new(bin("pg_dump"))
        .env("PGOPTIONS", "-c ferrum.system=on -c ferrum.vault=on")
        .args(["--enable-row-security", "--format=custom", "--file"])
        .arg(&file)
        .arg(&live)
        .output()
        .unwrap();
    assert!(dumped.status.success(), "{}", String::from_utf8_lossy(&dumped.stderr));
    // As deploy/dump.sh restores: without the schema the empty database has already, and comments on extensions.
    let listed = Command::new(bin("pg_restore")).arg("--list").arg(&file).output().unwrap();
    let kept: Vec<&str> =
        std::str::from_utf8(&listed.stdout).unwrap().lines().filter(|l| !l.contains(" COMMENT - EXTENSION ") && !l.contains(" SCHEMA - public ")).collect();
    std::fs::write(&list, kept.join("\n")).unwrap();
    let restored = Command::new(bin("pg_restore"))
        .args(["--no-owner", "--no-privileges", "--exit-on-error", "--single-transaction"])
        .arg(format!("--use-list={}", list.display()))
        .arg(format!("--dbname={copy}"))
        .arg(&file)
        .output()
        .unwrap();
    assert!(restored.status.success(), "{}", String::from_utf8_lossy(&restored.stderr));
    assert_eq!(partitioned(&copy).await, before, "every partition's rows came back");
    std::fs::remove_dir_all(&dir).ok();
}
