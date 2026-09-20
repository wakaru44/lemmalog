//! What a crash mid-`save` actually costs.
//!
//! Two claims, one per test, neither of which the round-trip suite touches
//! because neither can be reached without killing a process:
//!
//! 1. A crash in the window between `tx.commit()` and `checkpoint()` loses
//!    NOTHING — SQLite replays the WAL on the next open. What it does leave
//!    behind is a bare `.db` that is SHORT: an operator who copies the file
//!    without its `-wal`/`-shm` sidecars during that window gets the store
//!    as of the previous save. Closing that window is the entire job of the
//!    checkpoint.
//! 2. A `SIGKILL` landing anywhere — including inside a save — leaves a
//!    store that passes `integrity_check`, loads, and is never torn: the
//!    save is one transaction, so the `episodes` and `edges` tables always
//!    agree on how many turns the store has seen.
//!
//! Both need a real second process. The child is this same test binary
//! re-invoked on [`crash_child_helper`], which is a no-op unless
//! `LEMMALOG_CRASH_CHILD` names a mode — so a normal `cargo test` run just
//! sees it pass and do nothing.
#![cfg(all(feature = "sqlite", unix))]

use lemmalog::storage;
use lemmalog::{AgentMemory, MockExtractor};
use std::os::unix::process::ExitStatusExt;
use std::process::Command;
use std::time::{Duration, Instant};

type Mem = AgentMemory<MockExtractor>;

const MODE: &str = "LEMMALOG_CRASH_CHILD";
const STORE: &str = "LEMMALOG_CRASH_STORE";
const ABORT: &str = "LEMMALOG_CRASH_AFTER_COMMIT";

fn scratch(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("lemmalog-crash-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(name).to_str().unwrap().to_string();
    for suffix in ["", "-wal", "-shm", ".progress"] {
        let _ = std::fs::remove_file(format!("{path}{suffix}"));
    }
    path
}

fn progress_path(store: &str) -> String {
    format!("{store}.progress")
}

/// One turn per call: an episode row AND an edge row, from the only public
/// door into a memory. Each turn is a fresh subject, so nothing supersedes
/// anything and the two tables stay in lockstep — which is the invariant
/// the SIGKILL test leans on.
fn turns(m: &mut Mem, tag: &str, n: usize) {
    for i in 1..=n {
        let _ = m.observe_extracted(&format!("{tag}_{i} --lives_in--> city_{tag}_{i}"), 100);
    }
}

fn texts(m: &Mem) -> Vec<String> {
    m.episodes().iter().map(|e| e.text.clone()).collect()
}

fn edges(m: &Mem) -> usize {
    m.engine.relations["edge"].rows.len()
}

fn wal_len(store: &str) -> u64 {
    std::fs::metadata(format!("{store}-wal"))
        .map(|x| x.len())
        .unwrap_or(0)
}

/// Re-invoke this test binary on [`crash_child_helper`] in the given mode.
fn spawn_child(mode: &str, store: &str, abort_after_commit: bool) -> std::process::Child {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    // `--ignored` is how the helper is reachable at all: it is marked
    // #[ignore] so a normal `cargo test` run never enters it.
    cmd.args(["--exact", "crash_child_helper", "--ignored", "--test-threads=1"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .env(MODE, mode)
        .env(STORE, store);
    if abort_after_commit {
        cmd.env(ABORT, "1");
    } else {
        cmd.env_remove(ABORT);
    }
    cmd.spawn().expect("spawn child")
}

/// The child half of both tests: it saves a store and then dies, so it must
/// never run on its own. Two locks, because one of them silently lapsing is
/// how a store gets eaten: `#[ignore]` keeps a normal `cargo test` run out,
/// and the mode variable (set only by [`spawn_child`]) makes it a no-op even
/// if someone runs the ignored tests by hand.
#[test]
#[ignore = "child process entry point: spawned by the crash tests, never run directly"]
fn crash_child_helper() {
    let Ok(mode) = std::env::var(MODE) else {
        return;
    };
    let store = std::env::var(STORE).expect("child needs a store path");
    match mode.as_str() {
        // Load, append, save — and die inside `save`, between the commit
        // and the checkpoint, because the parent set LEMMALOG_CRASH_AFTER_COMMIT.
        "commit_abort" => {
            let mut m: Mem = storage::load(MockExtractor::new(0.9), &store).expect("child load");
            turns(&mut m, "crash", 5);
            storage::save(&m, &store).expect("child save");
            panic!("save returned: the abort hook did not fire");
        }
        // Save in a loop until someone kills us, leaving a breadcrumb that
        // says whether we were inside `save` when it happened.
        "kill_loop" => {
            let mut m: Mem = storage::load(MockExtractor::new(0.9), &store).expect("child load");
            let progress = progress_path(&store);
            let mut i = 0u64;
            loop {
                i += 1;
                // One episode row and one edge row per round: a torn save
                // would land one without the other. The tag counts from the
                // store's own size so subjects stay unique ACROSS killed
                // children — a repeated subject would re-assert the same
                // edge and cost an edge row without costing an episode.
                let tag = format!("r{}", m.episodes().len());
                turns(&mut m, &tag, 1);
                let _ = std::fs::write(&progress, format!("saving {i}"));
                storage::save(&m, &store).expect("child save");
                let _ = std::fs::write(&progress, format!("done {i}"));
            }
        }
        other => panic!("unknown child mode {other}"),
    }
}

/// Claim 1, both halves: the bare `.db` is short in the crash window (a),
/// and the store with its sidecars loses nothing (b).
#[test]
fn a_crash_before_the_checkpoint_shortens_the_bare_db_but_loses_nothing() {
    let store = scratch("commit-window.db");

    let mut m = AgentMemory::new(MockExtractor::new(0.9), "").unwrap();
    turns(&mut m, "first", 1);
    m.maintain(100);
    storage::save(&m, &store).unwrap();

    // Held open, and actually touching the file, for the rest of the test.
    // SQLite checkpoints when the LAST connection closes, which would fold
    // the WAL in behind our back and make (a) pass for the wrong reason.
    let reader = rusqlite::Connection::open(&store).unwrap();
    let seen: i64 = reader
        .query_row("SELECT count(*) FROM episodes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(seen, 1);

    // The last complete save: this is what a bare-`.db` copy must carry.
    turns(&mut m, "pre", 20);
    storage::save(&m, &store).unwrap();
    let before_crash = texts(&m);
    assert_eq!(wal_len(&store), 0, "a clean save folds the WAL in");

    let status = spawn_child("commit_abort", &store, true).wait().unwrap();
    assert_eq!(
        status.signal(),
        Some(6),
        "child must die by SIGABRT inside save, got {status:?}"
    );
    assert!(
        wal_len(&store) > 0,
        "the committed-but-unchecked-pointed save must still be in the WAL"
    );

    // (a) `cp store.db backup/` — sidecars left behind, as operators do.
    let copy = scratch("commit-window-copy.db");
    std::fs::copy(&store, &copy).unwrap();
    assert!(!std::path::Path::new(&format!("{copy}-wal")).exists());
    let from_copy: Mem = storage::load(MockExtractor::new(0.9), &copy).unwrap();
    assert_eq!(
        texts(&from_copy),
        before_crash,
        "the bare .db carries the last CHECKPOINTED save, no more and no less"
    );
    assert!(
        !texts(&from_copy).iter().any(|t| t.starts_with("crash_")),
        "the crashed save cannot be in a copy taken without the -wal"
    );

    // (b) the real store, sidecars intact: nothing was lost.
    let recovered: Mem = storage::load(MockExtractor::new(0.9), &store).unwrap();
    let mut expected = before_crash.clone();
    expected.extend(
        (1..=5).map(|i| format!("crash_{i} --lives_in--> city_crash_{i}")),
    );
    assert_eq!(
        texts(&recovered),
        expected,
        "reopening replays the WAL: the crashed save is there in full"
    );
    assert_eq!(
        edges(&recovered),
        edges(&m) + 5,
        "facts came back too, not just episodes"
    );
    drop(reader);
}

/// Claim 2: SIGKILL, landing inside a save often enough to matter, never
/// leaves a corrupt or half-written store.
#[test]
fn sigkill_during_a_save_leaves_an_intact_untorn_store() {
    const SEED_TURNS: usize = 3000;
    let store = scratch("sigkill.db");
    let progress = progress_path(&store);

    // Big enough that a save takes far longer than the child's startup,
    // so a delay in the tens of milliseconds can land inside one.
    let mut m = AgentMemory::new(MockExtractor::new(0.9), "").unwrap();
    turns(&mut m, "seed", SEED_TURNS);
    m.maintain(100);
    let base_episodes = m.episodes().len();
    let base_edges = edges(&m);
    let t = Instant::now();
    storage::save(&m, &store).unwrap();
    let save_ms = t.elapsed().as_millis();

    let mut inside_a_save = 0;
    // Rounds the child announced but whose save never committed: the kill
    // landed strictly INSIDE the transaction, not in the gap around it.
    let mut cut_mid_transaction = 0;
    let mut rounds_so_far = 0usize;
    let delays = [30, 45, 60, 75, 90, 110, 130, 150, 170, 190];
    for delay in delays {
        let _ = std::fs::remove_file(&progress);
        let mut child = spawn_child("kill_loop", &store, false);
        std::thread::sleep(Duration::from_millis(delay));
        child.kill().unwrap(); // std sends SIGKILL
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(9), "child must die by SIGKILL");

        let crumb = std::fs::read_to_string(&progress).unwrap_or_default();
        let announced: usize = crumb
            .rsplit(' ')
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        if crumb.starts_with("saving") {
            inside_a_save += 1;
        }

        let conn = rusqlite::Connection::open(&store).unwrap();
        let ok: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ok, "ok", "store corrupt after a kill at {delay}ms ({crumb})");
        drop(conn);

        let back: Mem = storage::load(MockExtractor::new(0.9), &store).unwrap();
        let rounds = back.episodes().len() - base_episodes;
        assert_eq!(
            edges(&back) - base_edges,
            rounds,
            "torn save after a kill at {delay}ms ({crumb}): {rounds} episodes past the \
             seed but {} edges — one save is one transaction, so these must agree",
            edges(&back) - base_edges
        );
        // The child's own rounds this time round, against what it said it
        // was doing when it died.
        let done_here = rounds - rounds_so_far;
        if crumb.starts_with("saving") && done_here + 1 == announced {
            cut_mid_transaction += 1;
        }
        rounds_so_far = rounds;
    }
    assert!(
        inside_a_save >= 2 && cut_mid_transaction >= 2,
        "the kill did not land inside a save often enough to prove anything \
         ({inside_a_save}/{} iterations inside `save`, {cut_mid_transaction} of them \
         with the transaction itself cut; one save takes ~{save_ms}ms) — retune the delays",
        delays.len()
    );
    eprintln!(
        "kill landed inside `save` in {inside_a_save}/{} iterations, cutting the \
         transaction itself {cut_mid_transaction} times (save ~{save_ms}ms)",
        delays.len()
    );
}
