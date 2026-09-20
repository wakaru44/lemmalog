//! lemmalog-cli: headless memory access on the same snapshot the MCP
//! server uses (`LEMMALOG_MCP_PATH`). For contexts where MCP tools are
//! unreachable — CLI sub-agents that don't inherit MCP connections,
//! scripts, cron jobs — every mutation here is visible to the next MCP
//! load and vice versa.
//!
//!   lemmalog-cli observe  --facts 'alice --works_at--> acme'
//!   lemmalog-cli query   --goal 'current("alice", R, O)'
//!   lemmalog-cli retract --facts 'alice --works_at--> acme'
//!   lemmalog-cli context --query 'where does alice work'
//!   lemmalog-cli why     --fact 'reports_to(alice, carol)'
//!   lemmalog-cli rules   --rules 'reach(X,Z) :- current(X,"dep",Y), reach(Y,Z).'
//!   lemmalog-cli dump    [--pred current]
//!
//! Concurrency: load → mutate → save is atomic-rename, but the CLI and a
//! live MCP server hold separate in-process copies — coordinate so only
//! one writes at a time (sub-agent runs while the parent only reads),
//! or have every writer use the CLI.
//!
//! Env: LEMMALOG_MCP_PATH (store path; required for mutations, optional
//! for fresh-session queries). A `.db`, `.sqlite` or `.sqlite3` extension
//! selects the SQLite store (needs the `sqlite` feature); anything else is
//! the tab-separated snapshot.

#![cfg(feature = "mcp")]

use lemmalog::agent::{AgentMemory, MockExtractor};
use std::io::Read;

fn snap_path() -> String {
    let path = std::env::var("LEMMALOG_MCP_PATH").unwrap_or_else(|_| {
        eprintln!("lemmalog-cli: set LEMMALOG_MCP_PATH to the shared store path");
        std::process::exit(2);
    });
    // Refuse before anything is opened: writing a snapshot into a file
    // whose name promises SQLite is worse than not running at all.
    if is_sqlite_store(&path) && !cfg!(feature = "sqlite") {
        eprintln!(
            "lemmalog-cli: {path:?} names a SQLite store but this binary was built without the \
             `sqlite` feature.\n  rebuild: cargo build --release --features mcp,sqlite\n  or set \
             LEMMALOG_MCP_PATH to a snapshot path (any extension but .db/.sqlite/.sqlite3)"
        );
        std::process::exit(2);
    }
    path
}

/// Which backend a store path selects: `.db`, `.sqlite` and `.sqlite3`
/// mean SQLite, anything else the tab-separated snapshot. The path is the
/// one knob an operator already sets, so it carries the choice — a second
/// mode flag would only be another thing to keep in sync with it.
///
/// This helper and the two wrappers below are duplicated verbatim in
/// `lemmalog-mcp.rs`. Sharing them would mean a new public module in
/// `src/lib.rs`; for three stateless functions the duplicate is the
/// narrower change (ADR 1).
fn is_sqlite_store(path: &str) -> bool {
    matches!(
        std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("db" | "sqlite" | "sqlite3")
    )
}

/// Load from whichever backend the path names.
///
/// A missing `.db` is not an error: `storage::load` opens it and creates
/// the schema (`CREATE TABLE IF NOT EXISTS`), so it reads back as an empty
/// store — the same "start fresh" outcome a missing snapshot gets.
fn store_load(path: &str) -> Result<AgentMemory<MockExtractor>, Box<dyn std::error::Error>> {
    #[cfg(feature = "sqlite")]
    if is_sqlite_store(path) {
        return lemmalog::storage::load(MockExtractor::new(0.9), path);
    }
    AgentMemory::load(MockExtractor::new(0.9), path)
}

/// Persist to whichever backend the path names.
fn store_save(
    m: &AgentMemory<MockExtractor>,
    path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "sqlite")]
    if is_sqlite_store(path) {
        return lemmalog::storage::save(m, path);
    }
    Ok(m.save(path)?)
}

/// Wall-clock seconds since the Unix epoch.
fn wall_clock() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Advance the engine clock to the present and re-derive temporal views.
///
/// `current/3` guards on `VF =< T < VT`, so a memory loaded at the
/// snapshot's stored `NOW` answers every read at that past instant: a
/// fact asserted later fails the `VF =< T` test and is invisible to
/// `current`, though it is still there in `edge`. Reads sync forward;
/// ingest keeps honouring its own `--ts` as valid-time. The clock only
/// ever moves forward — a backdated fact must not drag the present
/// backwards.
fn sync_clock(m: &mut AgentMemory<MockExtractor>) {
    let t = wall_clock();
    let e = &mut m.engine;
    if t > e.now {
        e.set_now(t);
        e.invalidate_derived();
        let _ = e.run();
    }
}

fn load(path: &str) -> AgentMemory<MockExtractor> {
    let mut m = match store_load(path) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("lemmalog-cli: store load failed ({e}); starting fresh");
            AgentMemory::new(MockExtractor::new(0.9), "").expect("fresh memory")
        }
    };
    sync_clock(&mut m);
    m
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned().filter(|v| !v.starts_with("--")))
}

fn stdin_or_flag(args: &[String], name: &str) -> String {
    if let Some(v) = flag(args, name) {
        return v;
    }
    let mut buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut buf);
    buf
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().cloned().unwrap_or_default();
    match cmd.as_str() {
        "observe" => {
            let mut m = load(&snap_path());
            let text = stdin_or_flag(&args, "--facts");
            let ts = flag(&args, "--ts").and_then(|t| t.parse::<i64>().ok());
            let ts = ts.unwrap_or_else(wall_clock);
            let (report, dropped) = m.observe_extracted(&text, ts);
            // observe_extracted sets the clock to `ts`, so an explicit
            // backdated --ts would otherwise persist a past NOW.
            sync_clock(&mut m);
            let _ = m.maintain(m.engine.now);
            store_save(&m, &snap_path()).expect("save store");
            println!(
                "added={} updated={} noop={} escalations={}",
                report.added,
                report.updated,
                report.noop,
                report.escalations.len()
            );
            for d in dropped.iter().take(5) {
                println!("dropped: {} ({})", d.0, d.1);
            }
        }
        "retract" => {
            let mut m = load(&snap_path());
            let text = stdin_or_flag(&args, "--facts");
            let (done, missing, died) = m.retract_facts(&text);
            store_save(&m, &snap_path()).expect("save store");
            println!("retracted {} fact(s)", done.len());
            if !died.is_empty() {
                println!("{} derived fact(s) died:", died.len());
                for d in died.iter().take(15) {
                    println!("  {d}");
                }
            }
            for mi in &missing {
                println!("not found: {mi}");
            }
        }
        "query" => {
            let m = load(&snap_path());
            let goal = stdin_or_flag(&args, "--goal");
            match m.ask(&goal) {
                Ok(rows) => {
                    if rows.is_empty() {
                        println!("(no answers — asserted facts are current(S, rel, O))");
                    } else {
                        println!("{}", rows.join("\n"));
                    }
                }
                Err(e) => {
                    eprintln!("parse: {e}\nhint: quote entity names — bare capitalized words are variables");
                    std::process::exit(1);
                }
            }
        }
        "context" => {
            let m = load(&snap_path());
            let query = stdin_or_flag(&args, "--query");
            let budget = flag(&args, "--budget")
                .and_then(|b| b.parse::<usize>().ok())
                .unwrap_or(1000);
            print!("{}", m.context_for_query_rich(&query, budget));
        }
        "why" => {
            let m = load(&snap_path());
            let fact = stdin_or_flag(&args, "--fact");
            println!("{}", m.why(&fact));
        }
        "rules" => {
            let mut m = load(&snap_path());
            let rules = stdin_or_flag(&args, "--rules");
            match m.install_rules(&rules) {
                Ok(id) => {
                    let n = m.maintain(m.engine.now);
                    store_save(&m, &snap_path()).expect("save store");
                    println!("installed {id}; backfill derived +{n} facts");
                    for w in m.batch_conflicts(&id) {
                        println!("WARNING: {w}");
                    }
                }
                Err(e) => {
                    eprintln!("install: {e}");
                    std::process::exit(1);
                }
            }
        }
        "rmrules" => {
            let mut m = load(&snap_path());
            let id = flag(&args, "--id").expect("--id <batch>");
            if m.uninstall_rules(&id) {
                let _ = m.maintain(m.engine.now);
                store_save(&m, &snap_path()).expect("save store");
                println!("uninstalled {id}; derivations reverted");
            } else {
                eprintln!("no batch {id:?} (see: lemmalog-cli batches)");
                std::process::exit(1);
            }
        }
        "batches" => {
            let m = load(&snap_path());
            for (id, src) in m.rule_batches() {
                println!("{id}: {}", src.lines().next().unwrap_or(""));
            }
        }
        "dump" => {
            let m = load(&snap_path());
            let pred = flag(&args, "--pred");
            let mut preds: Vec<&String> = m.engine.relations.keys().collect();
            preds.sort();
            for p in preds {
                if let Some(want) = &pred {
                    if p != want {
                        continue;
                    }
                }
                for key in m.engine.relation_keys(p) {
                    println!("{}", m.engine.render_fact(p, &key));
                }
            }
        }
        other => {
            eprintln!(
                "usage: lemmalog-cli observe|retract|query|context|why|rules|rmrules|batches|dump [flags]\n\
                 flags: --facts|--goal|--query|--fact|--rules|--id|--pred|--ts|--budget  (or stdin)\n\
                 unknown command {other:?}"
            );
            std::process::exit(2);
        }
    }
}
