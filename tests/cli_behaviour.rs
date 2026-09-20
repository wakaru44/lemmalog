//! What the BINARIES do, observed from outside: exit codes, stdout, stderr,
//! and the bytes and rows they leave behind.
//!
//! Everything else in `tests/` calls the library. Both defects pinned here
//! shipped and were caught by hand precisely because no test ever ran
//! `lemmalog-cli` and looked at what came out of it:
//!
//! 1. A fact an ontology refused printed `added=0` and nothing else. The
//!    reason sat unread in `IngestReport::rejected`, so an operator — or an
//!    agent — re-sent the same bad fact forever. The negative half matters
//!    just as much: with no ontology loaded the summary line must stay
//!    byte-for-byte what it was before the feature existed.
//! 2. Every load failure meant "start fresh", so a store that existed and
//!    could not be parsed was served as an empty memory and then written
//!    over by the next save. The schema-version guard spotted the hazard
//!    and the binary then destroyed the store it was guarding. The fix is
//!    exit 3 and write nothing — and the case that must NOT be swept up
//!    with it is the absent store, which is every first run there has ever
//!    been.
//!
//! `CARGO_BIN_EXE_*` is the binary cargo just built for this test run, so
//! there is no path to guess and no `cargo` to shell out to.
#![cfg(feature = "sqlite")]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const CLI: &str = env!("CARGO_BIN_EXE_lemmalog-cli");

/// A store path nobody else touches. The suite runs in parallel and two
/// upstream tests already share temp paths; one directory per test keeps
/// this file out of that.
fn store(test: &str, name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("cli-behaviour")
        .join(test);
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(name);
    // A leftover from a previous run would make every assertion below a
    // statement about that run instead of this one.
    for suffix in ["", "-wal", "-shm"] {
        let mut p = path.clone().into_os_string();
        p.push(suffix);
        let _ = std::fs::remove_file(p);
    }
    path
}

/// `lemmalog-cli <args>` against `path`, with the ontology explicitly set
/// or explicitly REMOVED — the no-ontology assertions mean nothing if an
/// inherited `LEMMALOG_ONTOLOGY` can sneak in from the ambient environment.
fn cli(path: &Path, ontology: Option<&Path>, args: &[&str]) -> Output {
    let mut cmd = Command::new(CLI);
    cmd.args(args).env("LEMMALOG_MCP_PATH", path);
    match ontology {
        Some(o) => cmd.env("LEMMALOG_ONTOLOGY", o),
        None => cmd.env_remove("LEMMALOG_ONTOLOGY"),
    };
    cmd.output().expect("run lemmalog-cli")
}

fn ontology() -> PathBuf {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("ontology.yaml");
    assert!(p.exists(), "the repo's ontology.yaml is the fixture: {p:?}");
    p
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}

/// The edge rows a store actually holds, read with SQL rather than through
/// the loader — the loader is the thing under test in half of these.
fn edge_rows(path: &Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(path).expect("open store");
    let mut q = conn
        .prepare("SELECT subject, predicate, object FROM edges ORDER BY id")
        .expect("prepare");
    let rows: Vec<String> = q
        .query_map([], |r| {
            Ok(format!(
                "{} --{}--> {}",
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?.unwrap_or_default()
            ))
        })
        .expect("query")
        .map(|r| r.expect("row"))
        .collect();
    rows
}

fn schema_version(path: &Path) -> Option<String> {
    let conn = rusqlite::Connection::open(path).expect("open store");
    conn.query_row(
        "SELECT value FROM meta WHERE key = 'schema_version'",
        [],
        |r| r.get(0),
    )
    .ok()
}

const BAD: &str = "svc:x --frobnicates--> svc:y";

// ---------------------------------------------------------------------
// Defect 1: an ontology rejection that said nothing.
// ---------------------------------------------------------------------

/// With an ontology loaded, a fact using a relation it does not declare is
/// counted AND explained on stdout, and never reaches the store.
#[test]
fn an_ontology_rejection_is_counted_explained_and_not_stored() {
    let path = store("ontology_reject", "x.db");
    let o = cli(&path, Some(&ontology()), &["observe", "--facts", BAD]);

    assert_eq!(o.status.code(), Some(0), "a rejection is not a crash");
    let stdout = out(&o);
    assert!(
        stdout.contains("added=0 updated=0 noop=0 escalations=0 rejected=1"),
        "the summary must carry the rejected count, got: {stdout:?}"
    );
    assert!(
        stdout.contains("rejected: svc:x --frobnicates--> svc:y (unknown relation `frobnicates`)"),
        "the per-fact reason must name the offending relation, got: {stdout:?}"
    );

    // Non-vacuous: the run did create and write a store, so an accepted
    // fact WOULD be visible here — see the sibling test, which finds it.
    assert!(path.exists(), "the run must have written a store to look in");
    assert_eq!(
        edge_rows(&path),
        Vec::<String>::new(),
        "a rejected fact must not be persisted"
    );
}

/// The negative, and the more fragile half: no ontology, same fact, and the
/// output must be exactly what it was before rejection existed — accepted,
/// with no `rejected=` anywhere on the line.
#[test]
fn without_an_ontology_the_same_fact_is_accepted_and_says_nothing_about_rejection() {
    let path = store("ontology_absent", "y.db");
    let o = cli(&path, None, &["observe", "--facts", BAD]);

    assert_eq!(o.status.code(), Some(0));
    let stdout = out(&o);
    assert_eq!(
        stdout.lines().next(),
        Some("added=1 updated=0 noop=0 escalations=0"),
        "with no ontology the summary line must be byte-identical to what it always was, \
         got: {stdout:?}"
    );
    assert!(
        !stdout.contains("rejected"),
        "no ontology, nothing to reject: {stdout:?}"
    );
    assert_eq!(
        edge_rows(&path),
        vec![BAD.to_string()],
        "the very fact the ontology refused is stored when no ontology is loaded"
    );
}

// ---------------------------------------------------------------------
// Defect 2: a store that could not be loaded was destroyed.
// ---------------------------------------------------------------------

/// (a) A store written at another schema version: refused with exit 3, and
/// still holding its rows and its old version afterwards.
#[test]
fn a_version_mismatched_store_is_refused_and_left_intact() {
    let path = store("version_mismatch", "v.db");
    let seed = cli(&path, None, &["observe", "--facts", "a --works_at--> b"]);
    assert_eq!(seed.status.code(), Some(0), "seeding must succeed");
    // Precondition: there IS something here to lose.
    assert_eq!(edge_rows(&path), vec!["a --works_at--> b".to_string()]);

    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("UPDATE meta SET value = '1' WHERE key = 'schema_version'", [])
            .unwrap();
    }
    assert_eq!(schema_version(&path).as_deref(), Some("1"));

    let o = cli(&path, None, &["observe", "--facts", "c --works_at--> d"]);
    assert_eq!(
        o.status.code(),
        Some(3),
        "a store that exists and will not load must stop the run; stderr: {}",
        err(&o)
    );
    assert!(
        err(&o).contains("exists but could not be loaded"),
        "the refusal must say why: {:?}",
        err(&o)
    );
    assert_eq!(out(&o), "", "a refused run prints no summary");

    assert_eq!(
        edge_rows(&path),
        vec!["a --works_at--> b".to_string()],
        "the guarded store must still hold its rows — this is the wipe that shipped"
    );
    assert_eq!(
        schema_version(&path).as_deref(),
        Some("1"),
        "and must not have been re-stamped at the current version"
    );
}

/// (b) A `.db` path holding something that is not a database at all: exit
/// 3, and the file identical byte for byte. Rows would be the wrong test
/// here — a wipe would leave a perfectly valid empty store.
#[test]
fn a_corrupt_store_is_refused_byte_for_byte() {
    let path = store("corrupt", "junk.db");
    let junk = b"this is not a database, it is a sentence\n".to_vec();
    std::fs::write(&path, &junk).unwrap();
    assert!(!junk.is_empty() && std::fs::read(&path).unwrap() == junk);

    let o = cli(&path, None, &["observe", "--facts", "a --works_at--> b"]);
    assert_eq!(
        o.status.code(),
        Some(3),
        "unparseable is not empty; stderr: {}",
        err(&o)
    );
    assert_eq!(out(&o), "", "a refused run prints no summary");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        junk,
        "the file must be untouched down to the byte"
    );
}

/// (c) The other direction, and the one an over-eager fix breaks: an ABSENT
/// store is not a damaged store. Every first run there has ever been lands
/// here.
#[test]
fn an_absent_db_still_starts_fresh() {
    let path = store("absent_db", "new.db");
    assert!(!path.exists(), "precondition: nothing is there yet");

    let o = cli(&path, None, &["observe", "--facts", "a --works_at--> b"]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "a first run must work; stderr: {}",
        err(&o)
    );
    assert!(out(&o).starts_with("added=1"), "got: {:?}", out(&o));
    assert_eq!(edge_rows(&path), vec!["a --works_at--> b".to_string()]);
}

/// And the snapshot backend is not collateral damage: a missing snapshot
/// still starts fresh and still writes.
#[test]
fn an_absent_snapshot_still_starts_fresh() {
    let path = store("absent_snapshot", "new.snapshot");
    assert!(!path.exists(), "precondition: nothing is there yet");

    let o = cli(&path, None, &["observe", "--facts", "a --works_at--> b"]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "a first run must work; stderr: {}",
        err(&o)
    );
    assert!(out(&o).starts_with("added=1"), "got: {:?}", out(&o));
    let written = std::fs::read_to_string(&path).expect("snapshot must exist after the run");
    assert!(
        written.contains("works_at"),
        "the fact must be in the snapshot: {written:?}"
    );
}
