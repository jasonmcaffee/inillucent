use inillucent_driver::Database;
#[test]
fn probe() {
    let mut p = std::env::temp_dir();
    p.push("inillucent-probe-derived.rdb");
    let _ = std::fs::remove_file(&p);
    let db = Database::open(&p).unwrap();
    let c = db.connect();
    for s in [
        "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT)",
        "INSERT INTO t VALUES (1,'one')",
        "INSERT INTO t VALUES (2,'two')",
        "CREATE TABLE u (a INTEGER PRIMARY KEY)",
        "INSERT INTO u VALUES (1)",
    ] {
        c.query(s, &[], 0).unwrap();
    }
    for s in [
        "SELECT x FROM (SELECT a AS x FROM t)",
        "SELECT x FROM (SELECT a AS x FROM t) WHERE x > 1",
        "SELECT d.x FROM (SELECT a AS x FROM t) AS d JOIN u ON d.x = u.a",
        "SELECT a FROM t UNION SELECT a FROM u",
        "SELECT (SELECT count(*) FROM u) FROM t",
        "CREATE VIEW v AS SELECT a FROM t",
        "SELECT a FROM v",
        "SELECT a, row_number() OVER (ORDER BY a) FROM t",
        "SELECT a FROM t WHERE a IN (SELECT a FROM u)",
        "SELECT a, (SELECT count(*) FROM u WHERE u.a = t.a) FROM t",
        "EXPLAIN SELECT a FROM t",
        "REINDEX",
        "VACUUM INTO 'copy.rdb'",
        "SELECT a FROM t ORDER BY a COLLATE NOCASE",
        "SELECT group_concat(b, '-') FROM t",
        "SELECT a FROM t LIMIT ?1",
        "SELECT json_extract('{\"a\":1}', '$.a')",
        "SELECT a FROM t EXCEPT SELECT a FROM u",
        "CREATE INDEX ix ON t (b)",
        "ALTER TABLE t RENAME TO t2",
    ] {
        match c.query(s, &[], 10) {
            Ok(r) => println!("OK    {:60} -> {} rows {:?}", s, r.total, r.rows),
            Err(e) => println!(
                "ERR   {:60} -> [{}] {} feature={:?}",
                s,
                e.status.name(),
                e.message,
                e.feature
            ),
        }
    }
    drop(c);
    drop(db);
    let _ = std::fs::remove_file(&p);
}
