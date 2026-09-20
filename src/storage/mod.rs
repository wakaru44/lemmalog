//! SQLite-backed store for [`AgentMemory`].
//!
//! Same persisted state, same semantics and the same signatures as the
//! tab-separated snapshot in [`crate::agent`] — only the container changes.
//! Persisted: the engine clock, `extra_rules`, every rule batch installed
//! past the bootstrap, episodes, escalations, and base (EDB) facts with
//! their annotations. Derived relations are rebuildable projections and
//! are NOT stored; `load` recomputes them with one maintenance run.
//!
//! The `edge` relation gets real columns (`edges` + `edge_prov`) so the
//! store is queryable by anything that speaks SQL; every other base
//! predicate lands n-ary and faithful in `facts` + `fact_args` +
//! `fact_prov`. The schema is `src/storage/schema.sql`.

use crate::agent::{
    AgentMemory, Episode, Extractor, RetractReason, BOOTSTRAP_BATCH, RETRACTED_AT_PROV,
    RETRACTED_BY_PROV, RETRACT_PROV,
};
use crate::eval::Ann;
use crate::intern::{Sym, Value};
use rusqlite::{params, Connection};

const SCHEMA: &str = include_str!("schema.sql");

type Res<T> = Result<T, Box<dyn std::error::Error>>;

fn open(path: &str) -> Res<Connection> {
    let conn = Connection::open(path)?;
    // WAL: ~200k facts go in as one transaction; rollback-journal fsync
    // behaviour makes that needlessly slow.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

/// An `edge` row destructured into subject, predicate, object and its
/// three timestamps, or `None` when the row does not have the
/// `edge(s,p,o,vf,vt,at)` shape — a mis-shaped row goes to the generic
/// tables rather than being dropped.
///
/// The object may be a symbol or an integer (`score --value_of--> 42`);
/// subject and predicate are always symbols. Without the integer case
/// those edges fell through to `facts`, which round-trips fine but leaves
/// `edges` an incomplete edge set — the opposite of why it exists.
fn as_edge(key: &[Value]) -> Option<([Sym; 2], Value, [i64; 3])> {
    let [Value::Sym(s), Value::Sym(p), o, Value::Int(vf), Value::Int(vt), Value::Int(at)] = key
    else {
        return None;
    };
    Some(([*s, *p], *o, [*vf, *vt, *at]))
}

/// Split a provenance set into (real provenance, retraction columns).
/// The markers are written to `edges` columns instead of `edge_prov`;
/// [`Retraction::markers`] puts them back on load.
fn split_retraction(prov: &std::collections::BTreeSet<String>) -> (Vec<&str>, Retraction) {
    let mut keep = Vec::new();
    let mut r = Retraction::default();
    for p in prov {
        if let Some(v) = p.strip_prefix(RETRACT_PROV) {
            r.reason = RetractReason::parse(v).map(|x| x.as_str().to_string());
        } else if let Some(v) = p.strip_prefix(RETRACTED_AT_PROV) {
            r.at = v.parse().ok();
        } else if let Some(v) = p.strip_prefix(RETRACTED_BY_PROV) {
            r.by = Some(v.to_string());
        } else {
            keep.push(p.as_str());
        }
    }
    (keep, r)
}

#[derive(Default)]
struct Retraction {
    reason: Option<String>,
    at: Option<i64>,
    by: Option<String>,
}

impl Retraction {
    /// The provenance markers these columns stand for — the inverse of
    /// [`split_retraction`].
    fn markers(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(r) = &self.reason {
            out.push(format!("{RETRACT_PROV}{r}"));
        }
        if let Some(at) = self.at {
            out.push(format!("{RETRACTED_AT_PROV}{at}"));
        }
        if let Some(by) = &self.by {
            out.push(format!("{RETRACTED_BY_PROV}{by}"));
        }
        out
    }
}

/// Persist to a SQLite store, replacing whatever it held.
pub fn save<X: Extractor>(m: &AgentMemory<X>, path: &str) -> Res<()> {
    let mut conn = open(path)?;
    let tx = conn.transaction()?;
    for t in [
        "edge_prov",
        "edges",
        "fact_args",
        "fact_prov",
        "facts",
        "episodes",
        "escalations",
        "rule_batches",
        "meta",
    ] {
        tx.execute(&format!("DELETE FROM {t}"), [])?;
    }

    {
        let mut meta = tx.prepare("INSERT INTO meta (key, value) VALUES (?1, ?2)")?;
        meta.execute(params!["now", m.engine.now.to_string()])?;
        meta.execute(params!["extra_rules", &m.extra_rules])?;

        // Batch ids are positional, so replay order is load-bearing; `ord`
        // is the primary key for exactly that reason.
        let mut batch = tx.prepare("INSERT INTO rule_batches (ord, id, src) VALUES (?1, ?2, ?3)")?;
        let mut ord = 0i64;
        for (id, src) in m.engine.batches() {
            if id == BOOTSTRAP_BATCH {
                continue; // reinstalled by `AgentMemory::new` from DEFAULT_RULES
            }
            batch.execute(params![ord, id, src])?;
            ord += 1;
        }

        let mut ep = tx.prepare("INSERT INTO episodes (id, ts, speaker, text) VALUES (?1,?2,?3,?4)")?;
        for e in &m.episodes {
            ep.execute(params![e.id, e.ts, e.speaker, e.text])?;
        }

        let mut esc = tx.prepare("INSERT INTO escalations (id, text) VALUES (?1, ?2)")?;
        for (i, e) in m.escalations.iter().enumerate() {
            esc.execute(params![i as i64, e])?;
        }

        let mut edge = tx.prepare(
            "INSERT INTO edges (subject, predicate, object_kind, object, object_int, \
             valid_from, valid_to, asserted_at, confidence, \
             retract_reason, retracted_at, retracted_by) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        )?;
        let mut edge_prov = tx.prepare("INSERT INTO edge_prov (edge_id, prov) VALUES (?1, ?2)")?;
        let mut fact = tx.prepare(
            "INSERT INTO facts (predicate, arity, confidence) VALUES (?1, ?2, ?3)",
        )?;
        let mut fact_arg = tx.prepare(
            "INSERT INTO fact_args (fact_id, pos, kind, sym, int_val) VALUES (?1,?2,?3,?4,?5)",
        )?;
        let mut fact_prov = tx.prepare("INSERT INTO fact_prov (fact_id, prov) VALUES (?1, ?2)")?;

        for (pred, rel) in &m.engine.relations {
            // base facts only: skip predicates defined by rules (they are
            // either program facts re-declared from rules, or derived)
            if m.engine.clauses.iter().any(|c| c.head.pred == *pred) {
                continue;
            }
            for row in &rel.rows {
                let ann = &row.fact.ann;
                match (pred.as_str(), as_edge(&row.key)) {
                    ("edge", Some((syms, obj, ints))) => {
                        let [s, p] = syms.map(|s| m.engine.interner.resolve(s).to_string());
                        let (kind, o_sym, o_int) = match obj {
                            Value::Sym(o) => (
                                "sym",
                                Some(m.engine.interner.resolve(o).to_string()),
                                None::<i64>,
                            ),
                            Value::Int(i) => ("int", None, Some(i)),
                        };
                        let (prov_keep, r) = split_retraction(&ann.prov);
                        edge.execute(params![
                            s,
                            p,
                            kind,
                            o_sym,
                            o_int,
                            ints[0],
                            ints[1],
                            ints[2],
                            ann.conf,
                            r.reason,
                            r.at,
                            r.by
                        ])?;
                        let id = tx.last_insert_rowid();
                        for prov in prov_keep {
                            edge_prov.execute(params![id, prov])?;
                        }
                    }
                    _ => {
                        fact.execute(params![pred, row.key.len() as i64, ann.conf])?;
                        let id = tx.last_insert_rowid();
                        for (pos, v) in row.key.iter().enumerate() {
                            match v {
                                Value::Sym(s) => fact_arg.execute(params![
                                    id,
                                    pos as i64,
                                    "sym",
                                    m.engine.interner.resolve(*s),
                                    None::<i64>
                                ])?,
                                Value::Int(i) => fact_arg.execute(params![
                                    id,
                                    pos as i64,
                                    "int",
                                    None::<String>,
                                    i
                                ])?,
                            };
                        }
                        for prov in &ann.prov {
                            fact_prov.execute(params![id, prov])?;
                        }
                    }
                }
            }
        }
    }

    tx.commit()?;
    Ok(())
}

/// Load a store into a fresh memory with the given extractor. Base facts
/// are re-asserted with their annotations; derived relations are rebuilt.
pub fn load<X: Extractor>(extractor: X, path: &str) -> Res<AgentMemory<X>> {
    let conn = open(path)?;

    let meta = |key: &str| -> Res<Option<String>> {
        let mut q = conn.prepare("SELECT value FROM meta WHERE key = ?1")?;
        let mut rows = q.query([key])?;
        Ok(match rows.next()? {
            Some(r) => Some(r.get(0)?),
            None => None,
        })
    };
    let now: i64 = meta("now")?.unwrap_or_default().parse().unwrap_or(0);
    let rules = meta("extra_rules")?.unwrap_or_default();

    let batch_srcs: Vec<String> = conn
        .prepare("SELECT src FROM rule_batches ORDER BY ord")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;

    // No ordinal column on `episodes`; (ts, id) is the stable order the
    // snapshot's insertion order carries anyway.
    let episodes: Vec<Episode> = conn
        .prepare("SELECT id, ts, speaker, text FROM episodes ORDER BY ts, id")?
        .query_map([], |r| {
            Ok(Episode {
                id: r.get(0)?,
                ts: r.get(1)?,
                speaker: r.get(2)?,
                text: r.get(3)?,
            })
        })?
        .collect::<Result<_, _>>()?;

    let escalations: Vec<String> = conn
        .prepare("SELECT text FROM escalations ORDER BY id")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;

    // (predicate, confidence, provenance, args)
    type Arg = (bool, String, i64);
    let mut facts: Vec<(String, f64, Vec<String>, Vec<Arg>)> = Vec::new();

    for row in conn
        .prepare(
            "SELECT id, subject, predicate, object_kind, object, object_int, \
             valid_from, valid_to, asserted_at, confidence, \
             retract_reason, retracted_at, retracted_by FROM edges ORDER BY id",
        )?
        .query_map([], |r| {
            let obj = if r.get::<_, String>(3)? == "sym" {
                (true, r.get::<_, String>(4)?, 0)
            } else {
                (false, String::new(), r.get::<_, i64>(5)?)
            };
            Ok((
                r.get::<_, i64>(0)?,
                vec![
                    (true, r.get::<_, String>(1)?, 0),
                    (true, r.get::<_, String>(2)?, 0),
                    obj,
                    (false, String::new(), r.get::<_, i64>(6)?),
                    (false, String::new(), r.get::<_, i64>(7)?),
                    (false, String::new(), r.get::<_, i64>(8)?),
                ],
                r.get::<_, f64>(9)?,
                Retraction {
                    reason: r.get::<_, Option<String>>(10)?,
                    at: r.get::<_, Option<i64>>(11)?,
                    by: r.get::<_, Option<String>>(12)?,
                },
            ))
        })?
    {
        let (id, args, conf, retraction) = row?;
        let mut p = prov(&conn, "edge_prov", "edge_id", id)?;
        p.extend(retraction.markers());
        facts.push(("edge".to_string(), conf, p, args));
    }

    for row in conn
        .prepare("SELECT id, predicate, confidence FROM facts ORDER BY id")?
        .query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, f64>(2)?))
        })?
    {
        let (id, pred, conf) = row?;
        let args: Vec<Arg> = conn
            .prepare("SELECT kind, sym, int_val FROM fact_args WHERE fact_id = ?1 ORDER BY pos")?
            .query_map([id], |r| {
                let kind: String = r.get(0)?;
                Ok((
                    kind == "sym",
                    r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    r.get::<_, Option<i64>>(2)?.unwrap_or_default(),
                ))
            })?
            .collect::<Result<_, _>>()?;
        facts.push((pred, conf, prov(&conn, "fact_prov", "fact_id", id)?, args));
    }

    // Load order is load-bearing: batch ids are positional, so replaying
    // non-bootstrap batches in order restores them under the same ids and
    // keeps `uninstall` working.
    let mut m = if batch_srcs.is_empty() {
        AgentMemory::new(extractor, &rules)?
    } else {
        // `extra_rules` became a batch at construction, so it is already in
        // `batch_srcs`; passing it again would install it twice.
        let mut m = AgentMemory::new(extractor, "")?;
        for src in &batch_srcs {
            m.engine.install_program(src)?;
        }
        m.extra_rules = rules;
        m
    };
    m.escalations = escalations;
    m.episodes = episodes;
    m.episode_counter = m.episodes.len() as u64;
    for (pred, conf, prov, args) in facts {
        let resolved: Vec<Value> = args
            .into_iter()
            .map(|(is_sym, s, i)| {
                if is_sym {
                    m.engine.sym(&s)
                } else {
                    Value::Int(i)
                }
            })
            .collect();
        m.engine.declare(&pred, &resolved, Ann::base(conf, prov));
    }
    m.engine.set_now(now);
    let _ = m.engine.run();
    m.last_turn_epoch = m.engine.epoch();
    Ok(m)
}

fn prov(conn: &Connection, table: &str, col: &str, id: i64) -> Res<Vec<String>> {
    Ok(conn
        .prepare(&format!("SELECT prov FROM {table} WHERE {col} = ?1 ORDER BY prov"))?
        .query_map([id], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::MockExtractor;

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!("lemmalog-store-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("store.db");
        let path = path.to_str().unwrap();
        let _ = std::fs::remove_file(path);

        let mut m = AgentMemory::new(MockExtractor::new(0.9), "").unwrap();
        m.engine.install_program("mgr(X,Y) :- likes(X,Y).").unwrap();
        let (a, mgr, b) = (m.engine.sym("alice"), m.engine.sym("manager"), m.engine.sym("bob"));
        m.engine.declare(
            "edge",
            &[a, mgr, b, Value::Int(0), Value::Int(i64::MAX), Value::Int(5)],
            Ann::base(0.8, ["ep1".to_string()]),
        );
        let c = m.engine.sym("carol");
        m.engine
            .declare("likes", &[a, c], Ann::base(0.5, ["ep2".to_string()]));
        m.engine
            .declare("score", &[a, Value::Int(42)], Ann::base(1.0, Vec::<String>::new()));
        m.engine.set_now(3);
        let _ = m.engine.run();

        let before = counts(&m);
        save(&m, path).unwrap();
        let back: AgentMemory<MockExtractor> = load(MockExtractor::new(0.9), path).unwrap();

        assert_eq!(counts(&back), before);
        assert_eq!(back.engine.now, m.engine.now);
        // annotations survive, not just tuples
        let e = &back.engine.relations["edge"].rows[0];
        assert_eq!(e.fact.ann.conf, 0.8);
        assert!(e.fact.ann.prov.contains("ep1"));
        assert_eq!(e.key[4], Value::Int(i64::MAX)); // open interval
        // the non-bootstrap batch replayed, so its derived view is back
        assert_eq!(back.engine.relations["mgr"].rows.len(), 1);
        assert_eq!(back.engine.batches().len(), m.engine.batches().len());
        // idempotent: saving over an existing store replaces it
        save(&back, path).unwrap();
        let again: AgentMemory<MockExtractor> = load(MockExtractor::new(0.9), path).unwrap();
        assert_eq!(counts(&again), before);
        let _ = std::fs::remove_file(path);
    }

    /// A gitignored scratch path, one per test.
    fn scratch(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("lemmalog-store-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(name).to_str().unwrap().to_string();
        let _ = std::fs::remove_file(&path);
        path
    }

    /// `edges` is only worth specializing if it is the COMPLETE edge set:
    /// an edge pointing at a number must land there, not fall through to
    /// the generic `facts` tables, and must come back an integer.
    #[test]
    fn int_object_edge_lands_in_the_edges_table() {
        let path = scratch("int-object.db");
        let mut m = AgentMemory::new(MockExtractor::new(0.9), "").unwrap();
        let (s, p) = (m.engine.sym("sensor_3"), m.engine.sym("reading"));
        m.engine.declare(
            "edge",
            &[
                s,
                p,
                Value::Int(42),
                Value::Int(0),
                Value::Int(i64::MAX),
                Value::Int(7),
            ],
            Ann::base(0.7, ["ep9".to_string()]),
        );
        m.engine.set_now(10);
        let _ = m.engine.run();
        save(&m, &path).unwrap();

        let conn = Connection::open(&path).unwrap();
        let ints: i64 = conn
            .query_row(
                "SELECT count(*) FROM edges WHERE object_kind = 'int' AND object_int = 42 \
                 AND object IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ints, 1, "the integer-object edge must be in `edges`");
        let strays: i64 = conn
            .query_row(
                "SELECT count(*) FROM facts WHERE predicate = 'edge'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(strays, 0, "no edge may fall through to `facts`");
        drop(conn);

        let back: AgentMemory<MockExtractor> = load(MockExtractor::new(0.9), &path).unwrap();
        let row = &back.engine.relations["edge"].rows[0];
        assert_eq!(row.key[2], Value::Int(42), "object stays an integer");
        assert_eq!(row.fact.ann.conf, 0.7);
        assert!(row.fact.ann.prov.contains("ep9"));
        let _ = std::fs::remove_file(&path);
    }

    /// The three retraction columns are written from the provenance
    /// markers and rebuilt from the columns on load.
    #[test]
    fn retraction_columns_round_trip() {
        let path = scratch("retraction.db");
        let mut m = AgentMemory::new(MockExtractor::new(0.9), "").unwrap();
        m.observe_extracted("alice --lives_in--> berlin", 100);
        m.maintain(100);
        m.engine.set_now(200);
        let (done, _, _) = m.retract_facts_because(
            "alice --lives_in--> berlin",
            RetractReason::WorldChanged,
            Some("tester"),
        );
        assert_eq!(done.len(), 1);
        save(&m, &path).unwrap();

        let conn = Connection::open(&path).unwrap();
        let (reason, at, by): (String, i64, String) = conn
            .query_row(
                "SELECT retract_reason, retracted_at, retracted_by FROM edges",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((reason.as_str(), at, by.as_str()), ("world_changed", 200, "tester"));
        let marker_rows: i64 = conn
            .query_row(
                "SELECT count(*) FROM edge_prov WHERE prov LIKE 'retract%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(marker_rows, 0, "markers live in columns, not in edge_prov");
        drop(conn);

        let back: AgentMemory<MockExtractor> = load(MockExtractor::new(0.9), &path).unwrap();
        let prov = &back.engine.relations["edge"].rows[0].fact.ann.prov;
        assert!(prov.contains("retract:world_changed"), "{prov:?}");
        assert!(prov.contains("retracted_at:200"), "{prov:?}");
        assert!(prov.contains("retracted_by:tester"), "{prov:?}");
        assert!(prov.contains("ep1"), "original provenance survives: {prov:?}");
        let _ = std::fs::remove_file(&path);
    }

    fn counts<X: Extractor>(m: &AgentMemory<X>) -> Vec<(String, usize)> {
        let mut v: Vec<(String, usize)> = m
            .engine
            .relations
            .iter()
            .map(|(p, r)| (p.clone(), r.rows.len()))
            .collect();
        v.sort();
        v
    }
}
