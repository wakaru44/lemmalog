//! lemmalog-cli: headless memory access on the same snapshot the MCP
//! server uses (`LEMMALOG_MCP_PATH`). For contexts where MCP tools are
//! unreachable — CLI sub-agents that don't inherit MCP connections,
//! scripts, cron jobs — every mutation here is visible to the next MCP
//! load and vice versa.
//!
//!   lemmalog-cli observe  --facts 'alice --works_at--> acme'
//!   lemmalog-cli query   --goal 'current("alice", R, O)'
//!   lemmalog-cli retract --facts 'alice --works_at--> acme' [--reason world_changed] [--by agent-7]
//!   lemmalog-cli suspects
//!   lemmalog-cli reverify --facts 'alice --manager--> bob'
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
use lemmalog::RetractReason;
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

/// Absent store -> start fresh. Present but unreadable -> refuse.
///
/// "Start fresh on any load error" is right for a first run and catastrophic
/// for a store that exists and cannot be parsed (schema mismatch, corruption):
/// the fresh memory is empty, and the next `store_save` writes it over the
/// file the load guard was protecting. Existence is the only thing that
/// separates the two cases, so it is what we branch on.
fn load(path: &str) -> AgentMemory<MockExtractor> {
    let mut m = match store_load(path) {
        Ok(m) => m,
        // A store that exists and will not load (schema mismatch, corruption)
        // must stop the run. Falling through to an empty memory is what made
        // this data loss: the memory is empty, and the very next `store_save`
        // writes it straight over the file the load guard was protecting.
        // Only absence is a legitimate "start fresh".
        Err(e) if std::path::Path::new(path).exists() => {
            eprintln!(
                "lemmalog-cli: {path:?} exists but could not be loaded ({e}).\n  Refusing to run: \
                 continuing with an empty memory would overwrite this store on the next save.\n  \
                 Move or delete the file, or point LEMMALOG_MCP_PATH elsewhere."
            );
            std::process::exit(3);
        }
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
            // `rejected=` is appended only when non-zero, so a run with no
            // ontology loaded prints byte-for-byte what it always printed.
            let rej = if report.rejected.is_empty() {
                String::new()
            } else {
                format!(" rejected={}", report.rejected.len())
            };
            println!(
                "added={} updated={} noop={} escalations={}{rej}",
                report.added,
                report.updated,
                report.noop,
                report.escalations.len()
            );
            // Each entry already reads "rejected: S --rel--> O (why)".
            for r in report.rejected.iter().take(5) {
                println!("{r}");
            }
            if report.rejected.len() > 5 {
                println!("(+{} more rejected)", report.rejected.len() - 5);
            }
            for d in dropped.iter().take(5) {
                println!("dropped: {} ({})", d.0, d.1);
            }
        }
        "retract" => {
            let mut m = load(&snap_path());
            // Default `wrong`: existing scripted callers keep today's
            // delete-and-kill-dependents behaviour byte for byte.
            let reason = match flag(&args, "--reason") {
                None => RetractReason::Wrong,
                Some(r) => match RetractReason::parse(&r) {
                    Some(r) => r,
                    None => {
                        eprintln!(
                            "lemmalog-cli: unknown --reason {r:?}; valid: wrong, world_changed, \
                             superseded"
                        );
                        std::process::exit(2);
                    }
                },
            };
            let text = stdin_or_flag(&args, "--facts");
            // `--by` is the retractor identity; absent, `None` keeps the
            // `retracted_by` column NULL exactly as before.
            let by = flag(&args, "--by");
            // `wrong` deletes the row, so there is no surviving fact to hang a
            // retractor on. Say so instead of dropping `--by` on the floor.
            if by.is_some() && reason == RetractReason::Wrong {
                eprintln!(
                    "lemmalog-cli: warning: --by is ignored with --reason wrong: the fact was \
                     never true, so the row is deleted and nothing survives to record the \
                     retractor. Use --reason world_changed to keep the earlier period true and \
                     record the retractor in edges.retracted_by."
                );
            }
            let (done, missing, died) = m.retract_facts_because(&text, reason, by.as_deref());
            // The closure markers only become facts the `suspect` rules can
            // join on when `maintain` lifts them out of provenance.
            if reason != RetractReason::Wrong {
                let _ = m.maintain(m.engine.now);
            }
            store_save(&m, &snap_path()).expect("save store");
            println!("retracted {} fact(s)", done.len());
            if reason != RetractReason::Wrong {
                println!(
                    "reason={}{}; {} fact(s) now suspect (see: lemmalog-cli suspects)",
                    reason.as_str(),
                    // the library scrubs commas out of `by` before it reaches
                    // provenance; echo what was asked for
                    by.as_deref().map(|b| format!(" by={b}")).unwrap_or_default(),
                    m.reverification_queue().len()
                );
            }
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
        "suspects" => {
            let mut m = load(&snap_path());
            // `maintain` is what lifts the retraction markers into the base
            // facts the suspect rules read: a store merely loaded reports an
            // empty queue until it runs.
            let _ = m.maintain(m.engine.now);
            let queue = m.reverification_queue();
            if queue.is_empty() {
                println!("re-verification queue empty");
            } else {
                println!("{} suspect fact(s):", queue.len());
                for s in &queue {
                    println!("  {}", s.fact);
                    println!(
                        "    closed support: {} (at {})",
                        s.closed_support, s.closed_at
                    );
                    for line in s.why.lines() {
                        println!("    why: {line}");
                    }
                }
                println!(
                    "clear one with: lemmalog-cli reverify --facts '<S --rel--> O>' \
                     (re-read current evidence first — this asserts confidence 1.0)"
                );
            }
        }
        "reverify" => {
            let mut m = load(&snap_path());
            let text = stdin_or_flag(&args, "--facts");
            let at = flag(&args, "--ts")
                .and_then(|t| t.parse::<i64>().ok())
                .unwrap_or_else(wall_clock);
            let (done, missing) = m.reverify(&text, at);
            // reverify() sets the clock to `at`; don't persist a past NOW.
            let _ = m.maintain(m.engine.now.max(wall_clock()));
            store_save(&m, &snap_path()).expect("save store");
            println!("re-verified {} fact(s) at confidence 1.0", done.len());
            for mi in &missing {
                println!("not found (no open fact matches): {mi}");
            }
            println!("{} fact(s) still suspect", m.reverification_queue().len());
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
                "usage: lemmalog-cli observe|retract|suspects|reverify|query|context|why|rules|rmrules|batches|dump [flags]\n\
                 flags: --facts|--goal|--query|--fact|--rules|--id|--pred|--ts|--budget|--reason|--by  (or stdin)\n\
                 retract --reason wrong (default: deletes, dependents die) | world_changed | superseded\n\
                 retract --by <who> records the retractor (edges.retracted_by); omitted = unrecorded\n\
                   (world_changed/superseded close the fact in valid time and mark dependents\n\
                    suspect — list them with `suspects`, clear them with `reverify`)\n\
                 unknown command {other:?}"
            );
            std::process::exit(2);
        }
    }
}
