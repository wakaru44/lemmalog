//! Differential round-trip tests for the SQLite store (`lemmalog::storage`)
//! against the reference semantics of the snapshot file
//! (`AgentMemory::save` / `AgentMemory::load`, src/agent.rs).
//!
//! Persisted state is exactly: engine clock `NOW`, `extra_rules`, rule
//! batches (minus the bootstrap `b0`), episodes, escalations, and base (EDB)
//! facts with `Ann { conf, prov }`. Derived facts are NOT persisted, so a
//! correct round-trip must *recompute* them — which is why every assertion
//! below looks at derived relations, `ask()` and `why()` as well as the base
//! rows.
#![cfg(feature = "sqlite")]

use lemmalog::storage;
use lemmalog::{AgentMemory, Ann, MockExtractor, Value};
use std::collections::{BTreeMap, BTreeSet};

type Mem = AgentMemory<MockExtractor>;

fn extractor() -> MockExtractor {
    MockExtractor::new(0.9)
}

/// Distinct, gitignored paths per format so the two round-trips can never
/// read each other's bytes.
fn scratch(name: &str) -> String {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/target/storage_roundtrip");
    std::fs::create_dir_all(dir).expect("create scratch dir");
    let p = format!("{dir}/{name}");
    let _ = std::fs::remove_file(&p);
    p
}

// ---------------------------------------------------------------- fixture

/// A memory exercising every persisted shape: several edge predicates, a
/// superseded (closed) edge, a non-`edge` base fact with an integer arg,
/// multiple episodes, an escalation, and an installed rule batch.
fn build_memory() -> Mem {
    let mut m = AgentMemory::new(extractor(), "").expect("new memory");

    // ep1: plain open edges, two predicates.
    m.observe_extracted(
        "alice --works_at--> acme\n\
         alice --manager--> bob\n\
         bob --works_at--> acme",
        100,
    );
    // ep2: multi-valued relation accumulates (two open `mentions` edges).
    m.observe_extracted(
        "ticket_7 --mentions--> alice\n\
         ticket_7 --mentions--> bob",
        200,
    );
    // ep3: FUNCTIONAL prefix (`status`) -> supersedes, closing the first
    // edge with valid_to = 300 instead of leaving both open.
    m.observe_extracted("ticket_7 --status_code--> open", 250);
    m.observe_extracted("ticket_7 --status_code--> closed", 300);
    // ep5: a non-functional, non-multi conflict -> escalation queue entry.
    m.observe_extracted("alice --manager--> carol", 350);

    // A base fact that is not an `edge`: different arity, integer argument,
    // explicit annotation. Exercises the generic fact path, not the
    // specialized edge one.
    let acme = m.engine.sym("acme");
    m.engine.declare(
        "headcount",
        &[acme, Value::Int(42)],
        Ann::base(0.75, ["hr-report"]),
    );

    // A rule batch installed after construction: not part of `extra_rules`,
    // so only the explicit batch replay can restore it.
    m.install_rules(
        "reports_to(X, Y) :- current(X, \"manager\", Y).\n\
         reports_to(X, Z) :- reports_to(X, Y), reports_to(Y, Z).\n\
         colleague(X, Y) :- current(X, \"works_at\", C), current(Y, \"works_at\", C).",
    )
    .expect("install rules");

    m.maintain(400);
    m
}

// ------------------------------------------------------------ fingerprint

/// One `edge` row, rendered in engine-independent terms: symbol ids differ
/// between two interners, so everything is resolved to text. Provenance is
/// a `BTreeSet<String>` upstream and is compared as a set, never as a
/// joined string.
type EdgeRow = (Vec<String>, String, Vec<String>);

#[derive(Debug, PartialEq, Eq)]
struct Fingerprint {
    now: i64,
    preds: BTreeSet<String>,
    counts: BTreeMap<String, usize>,
    edges: BTreeSet<EdgeRow>,
    episodes: Vec<(String, i64, Option<String>, String)>,
    escalations: Vec<String>,
    batches: Vec<(String, String)>,
}

fn fingerprint(m: &Mem) -> Fingerprint {
    let mut preds = BTreeSet::new();
    let mut counts = BTreeMap::new();
    let mut edges = BTreeSet::new();
    for (pred, rel) in &m.engine.relations {
        if rel.rows.is_empty() {
            // a relation that exists but is empty is indistinguishable from
            // one that was never created; do not let that be a difference
            continue;
        }
        preds.insert(pred.clone());
        counts.insert(pred.clone(), rel.rows.len());
        if pred == "edge" {
            for row in &rel.rows {
                let args: Vec<String> = row
                    .key
                    .iter()
                    .map(|v| m.engine.interner.display(v))
                    .collect();
                let prov: Vec<String> = row.fact.ann.prov.iter().cloned().collect();
                // f64 compared at storage precision, not bit-for-bit
                edges.insert((args, format!("{:.9}", row.fact.ann.conf), prov));
            }
        }
    }
    Fingerprint {
        now: m.engine.now,
        preds,
        counts,
        edges,
        episodes: m
            .episodes()
            .iter()
            .map(|e| (e.id.clone(), e.ts, e.speaker.clone(), e.text.clone()))
            .collect(),
        escalations: m.escalations().to_vec(),
        batches: m.rule_batches(),
    }
}

/// Sorted `current/3` rows — the derived view that only exists if both the
/// rules and the clock survived the round-trip.
fn current_rows(m: &Mem) -> Vec<String> {
    let mut rows = m.ask("current(E, R, O)").expect("current/3 query");
    rows.sort();
    rows
}

/// Guards every test against passing vacuously on an empty store.
fn assert_non_trivial(fp: &Fingerprint) {
    assert!(
        fp.edges.len() >= 6,
        "fixture must hold several edges, got {}",
        fp.edges.len()
    );
    assert!(
        fp.edges
            .iter()
            .any(|(args, _, _)| args[4] != i64::MAX.to_string()),
        "fixture must hold at least one closed (superseded) edge"
    );
    assert!(
        fp.edges
            .iter()
            .any(|(args, _, _)| args[4] == i64::MAX.to_string()),
        "fixture must hold at least one open edge"
    );
    assert_eq!(
        fp.counts.get("headcount").copied(),
        Some(1),
        "fixture must hold a non-edge base fact"
    );
    assert!(fp.episodes.len() >= 3, "fixture must hold several episodes");
    assert!(
        !fp.escalations.is_empty(),
        "fixture must hold at least one escalation"
    );
    assert!(
        fp.batches.len() >= 2,
        "fixture must hold the bootstrap batch plus an installed one"
    );
    assert!(fp.now > 0, "fixture must have advanced the clock");
    assert!(
        fp.counts.get("current").copied().unwrap_or(0) > 0,
        "fixture must derive current/3"
    );
}

// ------------------------------------------------------------------ tests

/// Proof 1: everything persisted comes back — clock, predicate set,
/// per-predicate counts, and the full `edge` row set with confidence and
/// provenance.
#[test]
fn sqlite_roundtrip_preserves_clock_predicates_and_edge_rows() {
    let m = build_memory();
    let before = fingerprint(&m);
    assert_non_trivial(&before);

    let path = scratch("fidelity.sqlite3");
    storage::save(&m, &path).expect("sqlite save");
    let loaded: Mem = storage::load(extractor(), &path).expect("sqlite load");
    let after = fingerprint(&loaded);

    assert_eq!(before.now, after.now, "engine clock must survive");
    assert_eq!(before.preds, after.preds, "predicate set must survive");
    assert_eq!(
        before.counts, after.counts,
        "per-predicate fact counts must survive"
    );
    assert_eq!(
        before.edges, after.edges,
        "every edge row, with confidence and provenance set, must survive"
    );
    assert_eq!(before.episodes, after.episodes, "episodes must survive");
    assert_eq!(
        before.escalations, after.escalations,
        "escalation queue must survive"
    );
    assert_eq!(
        before.batches, after.batches,
        "rule batch ids and sources must survive, in order"
    );
}

/// Proof 2: derived facts are not persisted, so `current/3` coming back
/// identical means the rules AND the clock were both restored.
#[test]
fn sqlite_roundtrip_recomputes_derived_current_rows() {
    let m = build_memory();
    let before = current_rows(&m);
    assert!(
        !before.is_empty(),
        "fixture must derive current/3 before saving"
    );

    let path = scratch("derived.sqlite3");
    storage::save(&m, &path).expect("sqlite save");
    let loaded: Mem = storage::load(extractor(), &path).expect("sqlite load");

    assert_eq!(
        before,
        current_rows(&loaded),
        "current/3 must be recomputed identically after load"
    );
    // the installed batch's transitive rule is derived state too
    let mut chain = loaded.ask("reports_to(\"alice\", Y)").expect("reports_to");
    chain.sort();
    let mut expected = m.ask("reports_to(\"alice\", Y)").expect("reports_to");
    expected.sort();
    assert!(!expected.is_empty(), "fixture must derive reports_to");
    assert_eq!(expected, chain, "installed rules must survive the load");
}

/// Proof 3: `why()` renders a proof tree back to source episodes. Identical
/// text after reload means provenance and supports were rebuilt the same.
#[test]
fn sqlite_roundtrip_preserves_why_proof_trees() {
    let m = build_memory();
    let goal = "current(alice, works_at, acme)";
    let before = m.why(goal);
    assert!(
        before.contains("ep1"),
        "fixture proof must reach its source episode: {before}"
    );

    let path = scratch("why.sqlite3");
    storage::save(&m, &path).expect("sqlite save");
    let loaded: Mem = storage::load(extractor(), &path).expect("sqlite load");

    assert_eq!(
        before,
        loaded.why(goal),
        "why() proof tree must be byte-identical after a round-trip"
    );
}

/// Proof 4: the actual differential. The same memory goes through the
/// upstream snapshot path and the SQLite path; the two loaded memories must
/// agree on everything. This is what catches the SQLite store quietly
/// diverging from the reference semantics.
#[test]
fn sqlite_and_snapshot_roundtrips_agree() {
    let m = build_memory();
    let origin = fingerprint(&m);
    assert_non_trivial(&origin);

    let snap_path = scratch("differential.snapshot");
    let db_path = scratch("differential.sqlite3");
    m.save(&snap_path).expect("snapshot save");
    storage::save(&m, &db_path).expect("sqlite save");

    let via_snapshot: Mem = AgentMemory::load(extractor(), &snap_path).expect("snapshot load");
    let via_sqlite: Mem = storage::load(extractor(), &db_path).expect("sqlite load");

    let a = fingerprint(&via_snapshot);
    let b = fingerprint(&via_sqlite);

    assert_eq!(a.now, b.now, "clock diverged between formats");
    assert_eq!(a.preds, b.preds, "predicate set diverged between formats");
    assert_eq!(a.counts, b.counts, "fact counts diverged between formats");
    assert_eq!(a.edges, b.edges, "edge rows diverged between formats");
    assert_eq!(a.episodes, b.episodes, "episodes diverged between formats");
    assert_eq!(
        a.escalations, b.escalations,
        "escalations diverged between formats"
    );
    assert_eq!(a.batches, b.batches, "rule batches diverged between formats");
    assert_eq!(
        current_rows(&via_snapshot),
        current_rows(&via_sqlite),
        "derived current/3 diverged between formats"
    );
    assert_eq!(
        via_snapshot.why("current(alice, works_at, acme)"),
        via_sqlite.why("current(alice, works_at, acme)"),
        "why() proof tree diverged between formats"
    );
    // and both must still agree with where they came from
    assert_eq!(origin.edges, b.edges, "sqlite diverged from the origin");
}

/// Proof 5: a realistically large store survives, and the wall clock is
/// reported. No threshold is asserted — machines vary — but the counts are.
#[test]
fn sqlite_roundtrip_survives_fifty_thousand_edges() {
    const N: usize = 50_000;
    let mut m = AgentMemory::new(extractor(), "").expect("new memory");
    // distinct subjects: each line is a fresh open edge, no supersede work
    let mut lines = String::with_capacity(N * 24);
    for i in 0..N {
        lines.push_str(&format!("entity_{i} --links_to--> target_{i}\n"));
    }
    let build = std::time::Instant::now();
    m.observe_extracted(&lines, 1_000);
    m.maintain(2_000);
    let build = build.elapsed();

    let edges_before = m.engine.relation_keys("edge").len();
    let current_before = m.engine.relation_keys("current").len();
    assert_eq!(edges_before, N, "fixture must hold {N} edges");
    assert_eq!(current_before, N, "fixture must derive {N} current facts");

    let path = scratch("scale.sqlite3");
    let t_save = std::time::Instant::now();
    storage::save(&m, &path).expect("sqlite save");
    let t_save = t_save.elapsed();

    let t_load = std::time::Instant::now();
    let loaded: Mem = storage::load(extractor(), &path).expect("sqlite load");
    let t_load = t_load.elapsed();

    assert_eq!(
        loaded.engine.relation_keys("edge").len(),
        edges_before,
        "all {N} edges must survive"
    );
    assert_eq!(
        loaded.engine.relation_keys("current").len(),
        current_before,
        "all {N} current facts must be recomputed"
    );
    assert_eq!(loaded.engine.now, m.engine.now, "clock must survive at scale");
    assert_eq!(loaded.episodes().len(), 1, "the episode must survive");

    let bytes = std::fs::metadata(&path).map(|md| md.len()).unwrap_or(0);
    println!(
        "scale: {N} edges | build {build:?} | save {t_save:?} | load {t_load:?} | {bytes} bytes"
    );
}
