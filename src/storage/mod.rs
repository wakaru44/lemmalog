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
    AgentMemory, AssertionContext, Episode, Extractor, FactClass, RetractReason, BOOTSTRAP_BATCH,
    BOUNDED_BY_PROV, RETRACTED_AT_PROV, RETRACTED_BY_PROV, RETRACT_PROV,
};
use crate::eval::Ann;
use crate::intern::{Sym, Value};
use rusqlite::{params, Connection};

const SCHEMA: &str = include_str!("schema.sql");

/// Shape of `schema.sql`. Bump on any change that makes a store written by
/// an older binary unreadable — there is no migration path, so `load`
/// refuses a mismatch instead of guessing. v2 added `episodes.ord`; v3
/// added `episodes.sha`, `.branch` and `.fact_class` (ADR 3).
const SCHEMA_VERSION: u32 = 3;

/// The version a store without a `schema_version` key was written at: the
/// key did not exist before v2, so its absence pins the store to v1.
const OLDEST_VERSION: u32 = 1;

const VERSION_KEY: &str = "schema_version";

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

/// Split a provenance set into (real provenance, `edges` columns). The
/// markers are written to columns instead of `edge_prov`;
/// [`EdgeCols::markers`] puts them back on load.
fn split_markers(prov: &std::collections::BTreeSet<String>) -> (Vec<&str>, EdgeCols) {
    let mut keep = Vec::new();
    let mut r = EdgeCols::default();
    for p in prov {
        if let Some(v) = p.strip_prefix(RETRACT_PROV) {
            r.reason = RetractReason::parse(v).map(|x| x.as_str().to_string());
        } else if let Some(v) = p.strip_prefix(RETRACTED_AT_PROV) {
            r.at = v.parse().ok();
        } else if let Some(v) = p.strip_prefix(RETRACTED_BY_PROV) {
            r.by = Some(v.to_string());
        } else if let Some(v) = p.strip_prefix(BOUNDED_BY_PROV) {
            r.bounded_by = Some(v.to_string());
        } else {
            keep.push(p.as_str());
        }
    }
    (keep, r)
}

/// The `edges` columns that ride the provenance set in memory: the three
/// retraction markers, plus how far back the search that produced a
/// `valid_from = i64::MIN` edge could actually look.
#[derive(Default)]
struct EdgeCols {
    reason: Option<String>,
    at: Option<i64>,
    by: Option<String>,
    bounded_by: Option<String>,
}

impl EdgeCols {
    /// The provenance markers these columns stand for — the inverse of
    /// [`split_markers`].
    fn markers(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(b) = &self.bounded_by {
            out.push(format!("{BOUNDED_BY_PROV}{b}"));
        }
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

/// Refuse a store written at a schema version that is not ours, before
/// anything reads or writes it. A store with no meta rows has never been
/// written — that is the "start fresh" case (the schema is created on
/// open), not a mismatch. Anything with meta in it was written by some
/// binary, and must say which.
fn check_version(conn: &Connection) -> Res<()> {
    let written: i64 = conn.query_row("SELECT count(*) FROM meta", [], |r| r.get(0))?;
    if written == 0 {
        return Ok(());
    }
    let mut q = conn.prepare("SELECT value FROM meta WHERE key = ?1")?;
    let mut rows = q.query([VERSION_KEY])?;
    let stored: Option<String> = match rows.next()? {
        Some(r) => Some(r.get(0)?),
        None => None,
    };
    let found = stored
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(OLDEST_VERSION);
    if found != SCHEMA_VERSION {
        return Err(version_error(found).into());
    }
    Ok(())
}

/// Persist to a SQLite store, replacing whatever it held.
pub fn save<X: Extractor>(m: &AgentMemory<X>, path: &str) -> Res<()> {
    let mut conn = open(path)?;
    // Before the transaction, and so before the nine DELETEs: a refused
    // save must leave the existing store untouched, not truncated.
    check_version(&conn)?;
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
        meta.execute(params![VERSION_KEY, SCHEMA_VERSION.to_string()])?;
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

        let mut ep = tx.prepare(
            "INSERT INTO episodes (ord, id, ts, speaker, text, sha, branch, fact_class) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        )?;
        for (i, e) in m.episodes.iter().enumerate() {
            let c = m.episode_context(&e.id);
            ep.execute(params![
                i as i64,
                e.id,
                e.ts,
                e.speaker,
                e.text,
                c.sha,
                c.branch,
                c.class.as_str()
            ])?;
        }

        let mut esc = tx.prepare("INSERT INTO escalations (id, text) VALUES (?1, ?2)")?;
        for (i, e) in m.escalations.iter().enumerate() {
            esc.execute(params![i as i64, e])?;
        }

        let mut edge = tx.prepare(
            "INSERT INTO edges (subject, predicate, object_kind, object, object_int, \
             valid_from, valid_to, asserted_at, confidence, \
             retract_reason, retracted_at, retracted_by, bounded_by, \
             asserted_at_sha, asserted_on_branch, fact_class) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
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
                        let (prov_keep, r) = split_markers(&ann.prov);
                        // Denormalised from the episode that asserted it,
                        // so the common query ("what did branch X claim?")
                        // is one indexable predicate and no join. The
                        // provenance set IS the set of episode ids; the
                        // first one we hold a context for wins, and a
                        // fact whose episodes are all unstamped (or gone)
                        // falls back to the default — NULL sha, NULL
                        // branch, fact_class 'agent' — which is exactly
                        // what an unstamped ingestion stores anyway.
                        let c = prov_keep
                            .iter()
                            .find_map(|id| m.episode_ctx.get(*id))
                            .cloned()
                            .unwrap_or_default();
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
                            r.by,
                            r.bounded_by,
                            c.sha,
                            c.branch,
                            c.class.as_str()
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
    // Test-only fault injection (tests/crash_consistency.rs): die in the
    // window between the commit and the checkpoint, the only place a crash
    // can leave the bare `.db` short of a save that already succeeded.
    // There is no other way to enter that window deterministically. Inert
    // unless the variable is set, which no binary or library path sets.
    if std::env::var_os("LEMMALOG_CRASH_AFTER_COMMIT").is_some() {
        std::process::abort();
    }
    // Only now, with the write durably committed: fold the WAL back into
    // the main file so `store.db` is complete on its own. Without this, an
    // operator who copies/backs up/commits just the `.db` — leaving the
    // `-wal` sidecar behind — silently loses the most recent saves.
    checkpoint(&conn);
    Ok(())
}

/// `PRAGMA wal_checkpoint(TRUNCATE)`: move every committed WAL frame into
/// the main database file and reset the `-wal` file to zero length.
///
/// Deliberately infallible. It runs *after* the commit, so the data is
/// already durable; a checkpoint that cannot complete (another connection
/// holding a read lock is the common case) is an on-disk tidiness miss,
/// not a lost write. Failing the save there would report a correctness
/// problem that does not exist and, worse, invite the caller to retry a
/// save that already succeeded. The result is swallowed rather than
/// propagated for exactly that reason; the store stays readable by any
/// SQLite client, `-wal` and all.
///
/// `wal_checkpoint` returns a row (busy, log frames, checkpointed
/// frames), so it needs the query form — `execute` rejects it.
fn checkpoint(conn: &Connection) {
    let _ = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
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
    check_version(&conn)?;

    let now: i64 = meta("now")?.unwrap_or_default().parse().unwrap_or(0);
    let rules = meta("extra_rules")?.unwrap_or_default();

    let batch_srcs: Vec<String> = conn
        .prepare("SELECT src FROM rule_batches ORDER BY ord")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;

    // `ord` is the in-memory Vec index: episode ids are positional, so the
    // original insertion order is the only correct one. (ts, id) is not a
    // substitute — one turn's facts share a ts, and `ep10` < `ep2` as TEXT.
    //
    // The episode is the source of truth for the assertion context: the
    // same columns on `edges` are a denormalised copy for SQL queries and
    // are deliberately NOT read back here.
    let mut episode_ctx: std::collections::HashMap<String, AssertionContext> =
        std::collections::HashMap::new();
    let mut episodes: Vec<Episode> = Vec::new();
    for row in conn
        .prepare("SELECT id, ts, speaker, text, sha, branch, fact_class FROM episodes ORDER BY ord")?
        .query_map([], |r| {
            Ok((
                Episode {
                    id: r.get(0)?,
                    ts: r.get(1)?,
                    speaker: r.get(2)?,
                    text: r.get(3)?,
                },
                AssertionContext {
                    sha: r.get(4)?,
                    branch: r.get(5)?,
                    // The CHECK constraint makes anything else
                    // unwritable; a store hand-edited past it reads as
                    // the default rather than failing the whole load.
                    class: FactClass::parse(&r.get::<_, String>(6)?).unwrap_or_default(),
                },
            ))
        })?
    {
        let (e, ctx) = row?;
        if ctx != AssertionContext::default() {
            episode_ctx.insert(e.id.clone(), ctx);
        }
        episodes.push(e);
    }

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
             retract_reason, retracted_at, retracted_by, bounded_by FROM edges ORDER BY id",
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
                EdgeCols {
                    reason: r.get::<_, Option<String>>(10)?,
                    at: r.get::<_, Option<i64>>(11)?,
                    by: r.get::<_, Option<String>>(12)?,
                    bounded_by: r.get::<_, Option<String>>(13)?,
                },
            ))
        })?
    {
        let (id, args, conf, cols) = row?;
        let mut p = prov(&conn, "edge_prov", "edge_id", id)?;
        p.extend(cols.markers());
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
    m.episode_ctx = episode_ctx;
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

/// What to tell the operator when a store's schema version is not ours.
/// Never a migration, never a silent load: the store is a projection that
/// can be rebuilt, and guessing at an old shape corrupts it quietly.
fn version_error(found: u32) -> String {
    if found < SCHEMA_VERSION {
        format!(
            "store schema version {found} is older than this binary's schema version \
             {SCHEMA_VERSION}; no migration path exists — delete this store and rebuild \
             it (save again from a snapshot or a running memory)"
        )
    } else {
        format!(
            "store schema version {found} is newer than this binary's schema version \
             {SCHEMA_VERSION}; upgrade the binary to one that understands schema version \
             {found} to read this store"
        )
    }
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

    /// Insertion order is the only correct episode order: ids are
    /// positional, and the two things that used to stand in for it both
    /// break here — a shared `ts`, and TEXT ids where `ep10` < `ep2`.
    #[test]
    fn episodes_keep_insertion_order_with_one_shared_ts() {
        let path = scratch("episode-order.db");
        let mut m = AgentMemory::new(MockExtractor::new(0.9), "").unwrap();
        for i in 1..=12 {
            m.episodes.push(Episode {
                id: format!("ep{i}"),
                ts: 500, // one turn: every episode shares a timestamp
                speaker: None,
                text: format!("line {i}"),
            });
        }
        m.episode_counter = m.episodes.len() as u64;
        let ids: Vec<String> = m.episodes.iter().map(|e| e.id.clone()).collect();
        save(&m, &path).unwrap();

        let back: AgentMemory<MockExtractor> = load(MockExtractor::new(0.9), &path).unwrap();
        let got: Vec<String> = back.episodes.iter().map(|e| e.id.clone()).collect();
        assert_eq!(got, ids, "episodes must come back in insertion order");
        assert_eq!(back.episodes[9].text, "line 10", "ep10 is still the 10th");
        assert_eq!(back.episode_counter, 12, "counter follows the episode count");
        let _ = std::fs::remove_file(&path);
    }

    /// Rewrite the version row of a real store, the way an older or newer
    /// binary would have left it.
    fn set_version(path: &str, v: Option<&str>) {
        let conn = Connection::open(path).unwrap();
        match v {
            Some(v) => conn
                .execute("UPDATE meta SET value = ?1 WHERE key = ?2", params![v, VERSION_KEY]),
            None => conn.execute("DELETE FROM meta WHERE key = ?1", params![VERSION_KEY]),
        }
        .unwrap();
    }

    /// `unwrap_err` needs `Debug` on the Ok side; `AgentMemory` has none.
    fn load_err(path: &str) -> String {
        match load(MockExtractor::new(0.9), path) {
            Ok(_) => panic!("load must refuse a version mismatch"),
            Err(e) => e.to_string(),
        }
    }

    fn saved_store(name: &str) -> String {
        let path = scratch(name);
        let mut m = AgentMemory::new(MockExtractor::new(0.9), "").unwrap();
        m.observe_extracted("alice --lives_in--> berlin", 100);
        m.maintain(100);
        save(&m, &path).unwrap();
        path
    }

    #[test]
    fn matching_version_loads_and_mismatches_are_errors() {
        // (f) the ordinary path still works end to end.
        let path = saved_store("version-ok.db");
        let back: AgentMemory<MockExtractor> = load(MockExtractor::new(0.9), &path).unwrap();
        assert_eq!(back.episodes.len(), 1);
        assert_eq!(back.engine.relations["edge"].rows.len(), 1);

        // (c) older version: rebuild, no migration.
        set_version(&path, Some("1"));
        let e = load_err(&path);
        assert!(e.contains("version 1 is older"), "{e}");
        assert!(e.contains(&SCHEMA_VERSION.to_string()), "{e}");
        assert!(e.contains("no migration path") && e.contains("rebuild"), "{e}");

        // (d) no version key at all (written before the guard existed):
        // same actionable error, not a confusing SQL failure.
        set_version(&path, None);
        let missing = load_err(&path);
        assert_eq!(missing, e, "a keyless store is the oldest known version");

        // (e) newer version: upgrade the binary.
        let path = saved_store("version-newer.db");
        set_version(&path, Some(&(SCHEMA_VERSION + 1).to_string()));
        let e = load_err(&path);
        assert!(e.contains("is newer") && e.contains("upgrade the binary"), "{e}");

        // A store that was never written is "start fresh", not a mismatch.
        let fresh = scratch("version-fresh.db");
        let empty: AgentMemory<MockExtractor> = load(MockExtractor::new(0.9), &fresh).unwrap();
        assert!(empty.episodes.is_empty());
        let _ = std::fs::remove_file(&fresh);
    }

    /// Same shape as [`load_err`]: `save` is refused, never panics.
    fn save_err<X: Extractor>(m: &AgentMemory<X>, path: &str) -> String {
        match save(m, path) {
            Ok(()) => panic!("save must refuse a version mismatch"),
            Err(e) => e.to_string(),
        }
    }

    /// A refused save must not have touched the store, so compare the file
    /// itself — row counts would still pass if the DELETEs had run and been
    /// rolled back into a rewritten file.
    fn bytes(path: &str) -> Vec<u8> {
        std::fs::read(path).unwrap()
    }

    /// (a)+(b) an older store refuses the save with the rebuild message and
    /// the file comes out byte-for-byte as it went in.
    #[test]
    fn save_into_an_older_store_is_refused_and_changes_nothing() {
        let path = saved_store("save-older.db");
        set_version(&path, Some("1"));
        let before = bytes(&path);

        let mut m = AgentMemory::new(MockExtractor::new(0.9), "").unwrap();
        m.observe_extracted("bob --lives_in--> madrid", 100);
        m.maintain(100);
        let e = save_err(&m, &path);

        assert!(e.contains("version 1 is older"), "{e}");
        assert!(e.contains("no migration path") && e.contains("rebuild"), "{e}");
        assert_eq!(e, load_err(&path), "save and load must say the same thing");
        assert_eq!(bytes(&path), before, "a refused save must not touch the file");
        let _ = std::fs::remove_file(&path);
    }

    /// (c) a store from a newer binary: upgrade, do not clobber.
    #[test]
    fn save_into_a_newer_store_is_refused_and_changes_nothing() {
        let path = saved_store("save-newer.db");
        set_version(&path, Some(&(SCHEMA_VERSION + 1).to_string()));
        let before = bytes(&path);

        let m = AgentMemory::new(MockExtractor::new(0.9), "").unwrap();
        let e = save_err(&m, &path);

        assert!(e.contains("is newer") && e.contains("upgrade the binary"), "{e}");
        assert_eq!(e, load_err(&path));
        assert_eq!(bytes(&path), before);
        let _ = std::fs::remove_file(&path);
    }

    /// (d) a pre-guard store has no version key at all; it is the oldest
    /// known version, so it gets the rebuild message too.
    #[test]
    fn save_into_a_keyless_store_is_refused_and_changes_nothing() {
        let path = saved_store("save-keyless.db");
        set_version(&path, None);
        let before = bytes(&path);

        let m = AgentMemory::new(MockExtractor::new(0.9), "").unwrap();
        let e = save_err(&m, &path);

        assert!(e.contains("no migration path") && e.contains("rebuild"), "{e}");
        assert_eq!(e, load_err(&path));
        assert_eq!(bytes(&path), before);
        let _ = std::fs::remove_file(&path);
    }

    /// (e)+(f) the normal first save writes the current version, and the
    /// store it leaves behind round-trips.
    #[test]
    fn first_save_stamps_the_current_version_and_round_trips() {
        let path = saved_store("save-fresh.db");
        let stamped: String = Connection::open(&path)
            .unwrap()
            .query_row("SELECT value FROM meta WHERE key = ?1", [VERSION_KEY], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(stamped, SCHEMA_VERSION.to_string());

        let back: AgentMemory<MockExtractor> = load(MockExtractor::new(0.9), &path).unwrap();
        assert_eq!(back.episodes.len(), 1);
        assert_eq!(back.engine.relations["edge"].rows.len(), 1);
        save(&back, &path).unwrap(); // a matching version saves again fine
        let _ = std::fs::remove_file(&path);
    }

    /// The point of the checkpoint: a `.db` copied WITHOUT its `-wal` and
    /// `-shm` sidecars must still hold everything the last save wrote.
    /// This is the copy/backup/commit path an operator actually takes.
    #[test]
    fn the_db_file_alone_carries_the_last_save() {
        let path = scratch("self-contained.db");
        let mut m = AgentMemory::new(MockExtractor::new(0.9), "").unwrap();
        m.observe_extracted("alice --lives_in--> berlin", 100);
        m.maintain(100);
        save(&m, &path).unwrap(); // the store as it stood before the writes below

        // A second connection on the store, held open (and actually
        // touching the file — SQLite opens lazily) across the save that
        // follows. Not decoration: SQLite checkpoints on the close of the
        // LAST connection, which would hide the bug entirely. A live
        // reader — the MCP server, a `sqlite3` shell — is also the
        // realistic state of the world when an operator reaches for `cp`.
        let reader = Connection::open(&path).unwrap();
        let seen: i64 = reader
            .query_row("SELECT count(*) FROM episodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(seen, 1);

        for i in 1..=40 {
            m.episodes.push(Episode {
                id: format!("bulk{i}"),
                ts: 100,
                speaker: None,
                text: format!("padding line {i}"),
            });
        }
        m.episode_counter = m.episodes.len() as u64;
        save(&m, &path).unwrap();

        // TRUNCATE leaves the sidecar at zero length (or gone): either way
        // it carries nothing, which is the whole claim.
        let wal_len = std::fs::metadata(format!("{path}-wal"))
            .map(|x| x.len())
            .unwrap_or(0);
        assert_eq!(wal_len, 0, "the WAL must be empty after a successful save");

        // Copy ONLY the .db, the way `cp store.db backup/` does.
        let copy = scratch("self-contained-copy.db");
        std::fs::copy(&path, &copy).unwrap();
        assert!(!std::path::Path::new(&format!("{copy}-wal")).exists());

        let back: AgentMemory<MockExtractor> = load(MockExtractor::new(0.9), &copy).unwrap();
        assert_eq!(back.episodes.len(), m.episodes.len(), "episodes survived the copy");
        assert_eq!(back.episodes[40].id, "bulk40");
        assert_eq!(
            back.engine.relations["edge"].rows.len(),
            m.engine.relations["edge"].rows.len(),
            "edges survived the copy"
        );
        drop(reader);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&copy);
    }

    /// A refused save must not checkpoint either: the file stays
    /// byte-identical, and nothing of the caller's memory reaches the WAL.
    #[test]
    fn a_refused_save_does_not_checkpoint() {
        let path = saved_store("refused-no-checkpoint.db");
        set_version(&path, Some(&(SCHEMA_VERSION + 1).to_string()));
        let before = bytes(&path);

        let mut m = AgentMemory::new(MockExtractor::new(0.9), "").unwrap();
        m.observe_extracted("bob --lives_in--> madrid", 100);
        m.maintain(100);
        let e = save_err(&m, &path);
        assert!(e.contains("is newer"), "{e}");

        assert_eq!(bytes(&path), before, "a refused save must not touch the file");
        let wal_len = std::fs::metadata(format!("{path}-wal"))
            .map(|x| x.len())
            .unwrap_or(0);
        assert_eq!(wal_len, 0, "a refused save writes nothing, so nothing to fold in");
        let _ = std::fs::remove_file(&path);
    }

    /// (sha, branch, fact_class) of every edge, ordered by subject, read
    /// back out of SQL — the columns exist to be queried that way, so the
    /// tests query them that way rather than trusting the in-memory side.
    fn edge_context_rows(path: &str) -> Vec<(Option<String>, Option<String>, String)> {
        let conn = Connection::open(path).unwrap();
        let mut q = conn
            .prepare(
                "SELECT asserted_at_sha, asserted_on_branch, fact_class FROM edges \
                 ORDER BY subject",
            )
            .unwrap();
        let rows = q
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        rows
    }

    /// (a) Nothing set: the ADR-3 columns stay NULL and the class is the
    /// default, on the episode and on the edge alike. This is the shape
    /// every store written before the feature existed has.
    #[test]
    fn without_an_assertion_context_the_columns_are_null() {
        let path = scratch("ctx-unset.db");
        let mut m = AgentMemory::new(MockExtractor::new(0.9), "").unwrap();
        m.observe_extracted("alice --lives_in--> berlin", 100);
        m.maintain(100);
        assert_eq!(m.assertion_context(), &AssertionContext::default());
        save(&m, &path).unwrap();

        assert_eq!(edge_context_rows(&path), vec![(None, None, "agent".into())]);
        let conn = Connection::open(&path).unwrap();
        let ep: (Option<String>, Option<String>, String) = conn
            .query_row("SELECT sha, branch, fact_class FROM episodes", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(ep, (None, None, "agent".to_string()));
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    /// (b) With a context set, the episode carries it AND the edge row
    /// carries the denormalised copy — the copy is the whole point, since
    /// it is what makes `WHERE asserted_on_branch = ?` a join-free query.
    #[test]
    fn a_set_context_reaches_the_episode_and_the_edge_row() {
        let path = scratch("ctx-set.db");
        let mut m = AgentMemory::new(MockExtractor::new(0.9), "").unwrap();
        m.set_assertion_context(Some("cafe1234"), Some("main"), Some("human"))
            .unwrap();
        m.observe_extracted("alice --lives_in--> berlin", 100);
        m.maintain(100);
        save(&m, &path).unwrap();

        let ctx = m.episode_context("ep1");
        assert_eq!(ctx.sha.as_deref(), Some("cafe1234"));
        assert_eq!(ctx.branch.as_deref(), Some("main"));
        assert_eq!(ctx.class, FactClass::Human);

        assert_eq!(
            edge_context_rows(&path),
            vec![(Some("cafe1234".into()), Some("main".into()), "human".into())],
            "the edge row must carry the episode's context, denormalised"
        );
        let conn = Connection::open(&path).unwrap();
        let ep: (Option<String>, String) = conn
            .query_row("SELECT sha, fact_class FROM episodes", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(ep, (Some("cafe1234".to_string()), "human".to_string()));
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    /// (c) Two ingestions under two contexts: the sha follows the EPISODE
    /// that asserted each edge, not the memory. A global "last context
    /// wins" implementation gives both edges the same sha and fails here.
    #[test]
    fn each_episode_keeps_the_context_it_was_ingested_under() {
        let path = scratch("ctx-per-episode.db");
        let mut m = AgentMemory::new(MockExtractor::new(0.9), "").unwrap();
        m.set_assertion_context(Some("aaaa1111"), Some("main"), None)
            .unwrap();
        m.observe_extracted("alice --lives_in--> berlin", 100);
        m.set_assertion_context(Some("bbbb2222"), Some("topic"), Some("machine"))
            .unwrap();
        m.observe_extracted("zoe --lives_in--> lisbon", 101);
        m.maintain(101);
        save(&m, &path).unwrap();

        assert_eq!(
            edge_context_rows(&path),
            vec![
                (Some("aaaa1111".into()), Some("main".into()), "agent".into()),
                (Some("bbbb2222".into()), Some("topic".into()), "machine".into()),
            ],
            "alice's edge came from ep1, zoe's from ep2"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// (d) The context survives save -> load -> save: the episode is the
    /// source of truth on the way back in, and re-saving reproduces the
    /// same denormalised edge columns.
    #[test]
    fn assertion_context_survives_a_round_trip() {
        let path = scratch("ctx-round-trip.db");
        let mut m = AgentMemory::new(MockExtractor::new(0.9), "").unwrap();
        m.set_assertion_context(Some("deadbeef"), Some("w44/feat"), Some("machine"))
            .unwrap();
        m.observe_extracted("alice --lives_in--> berlin", 100);
        m.maintain(100);
        save(&m, &path).unwrap();
        let before = edge_context_rows(&path);

        let back: AgentMemory<MockExtractor> = load(MockExtractor::new(0.9), &path).unwrap();
        let ctx = back.episode_context("ep1");
        assert_eq!(ctx.sha.as_deref(), Some("deadbeef"));
        assert_eq!(ctx.branch.as_deref(), Some("w44/feat"));
        assert_eq!(ctx.class, FactClass::Machine);

        let again = scratch("ctx-round-trip-2.db");
        save(&back, &again).unwrap();
        assert_eq!(edge_context_rows(&again), before);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&again);
    }

    /// (f) `bounded_by` is wired end to end: set on an edge that predates
    /// recorded history (`valid_from = i64::MIN`), it reaches the column
    /// and comes back. NOTHING populates it yet — the pickaxe backfill
    /// that would is deferred (ADR 3) — so this is the only path that
    /// proves the column is not write-only.
    #[test]
    fn bounded_by_round_trips_with_an_open_left_interval() {
        let path = scratch("bounded-by.db");
        let mut m = AgentMemory::new(MockExtractor::new(0.9), "").unwrap();
        let (s, p, o) = (m.engine.sym("monolith"), m.engine.sym("uses"), m.engine.sym("smarty"));
        m.engine.declare(
            "edge",
            &[
                s,
                p,
                o,
                Value::Int(i64::MIN), // predates recorded history
                Value::Int(i64::MAX),
                Value::Int(5),
            ],
            Ann::base(
                0.9,
                [
                    "ep1".to_string(),
                    format!("{BOUNDED_BY_PROV}first-commit-2014"),
                ],
            ),
        );
        m.engine.set_now(10);
        let _ = m.engine.run();
        save(&m, &path).unwrap();

        let conn = Connection::open(&path).unwrap();
        let (bounded, vf): (Option<String>, i64) = conn
            .query_row("SELECT bounded_by, valid_from FROM edges", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(bounded.as_deref(), Some("first-commit-2014"));
        assert_eq!(vf, i64::MIN, "the left-open sentinel is what it bounds");
        let stray: i64 = conn
            .query_row("SELECT count(*) FROM edge_prov WHERE prov LIKE 'bounded_by:%'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(stray, 0, "the marker becomes a column, not a prov row");
        drop(conn);

        let back: AgentMemory<MockExtractor> = load(MockExtractor::new(0.9), &path).unwrap();
        let e = &back.engine.relations["edge"].rows[0];
        assert!(
            e.fact.ann.prov.contains(&format!("{BOUNDED_BY_PROV}first-commit-2014")),
            "the column must rebuild the marker: {:?}",
            e.fact.ann.prov
        );
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
