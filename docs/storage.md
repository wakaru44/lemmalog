# Storage backends — operator guide

Lemmalog persists a memory to one of two backends: the original
tab-separated **snapshot** file, or a **SQLite** store. This page is for
whoever runs the MCP server or the CLI: choosing a backend, building the
right binary, reading the data with `sqlite3`, and moving an existing
snapshot setup over.

Everything below was run against a real store. The examples use a single
store path, `memory.db`, and the facts are generic service/table edges.

---

## 1. Choosing a backend

One knob: **`LEMMALOG_MCP_PATH`**. Both binaries (`lemmalog-mcp`,
`lemmalog-cli`) read it, and the **file extension** picks the backend:

| Path ends in | Backend |
| --- | --- |
| `.db`, `.sqlite`, `.sqlite3` | SQLite store |
| anything else | tab-separated snapshot file |

There is no separate mode flag and no config file. To switch backends you
change the path.

```sh
export LEMMALOG_MCP_PATH=/var/lib/lemmalog/memory.db        # SQLite
export LEMMALOG_MCP_PATH=/var/lib/lemmalog/memory.snapshot  # snapshot
```

A missing `.db` is not an error — the first write creates and initialises
it.

## 2. Building

The SQLite backend is behind the `sqlite` cargo feature. The binaries also
need `mcp`:

```sh
cargo build --release --features mcp,sqlite
```

A binary built without `sqlite` **refuses** a SQLite-looking path at
startup. It does not fall back, and it does not write a snapshot into a
file called `.db`:

```console
$ LEMMALOG_MCP_PATH=probe.db ./target/release/lemmalog-cli query --goal 'current(S,R,O)'
lemmalog-cli: "probe.db" names a SQLite store but this binary was built without the `sqlite` feature.
  rebuild: cargo build --release --features mcp,sqlite
  or set LEMMALOG_MCP_PATH to a snapshot path (any extension but .db/.sqlite/.sqlite3)
$ echo $?
2
```

No file is created by that run. Non-zero exit, nothing on disk, nothing
silently written in the wrong format.

## 3. Why the SQLite backend exists

The snapshot file is rewritten **whole** on every mutation. Three
consequences an operator feels:

- **Concurrent writers erase each other.** Two processes pointed at the
  same snapshot both load, both mutate, both rewrite the entire file. The
  last writer wins and the other's facts are simply gone — no error, no
  conflict.
- **Readers are stale until restart.** A reader holds the memory it loaded
  at startup; new facts written by someone else are invisible until it
  reloads the file.
- **Git-tracked stores diff badly.** Because the file is rewritten end to
  end, every commit is a full-file diff. Review is impractical and merges
  conflict on unrelated facts.

The practical workaround with the snapshot is a **single-writer
discipline**: exactly one process is allowed to write, and everyone else
hands it facts. That discipline is what SQLite removes. The store is opened
in WAL mode, so readers do not block the writer and the writer does not
block readers:

```console
$ sqlite3 memory.db "PRAGMA journal_mode;"
wal
```

WAL also means the store is **three files** on disk — `memory.db`,
`memory.db-wal`, `memory.db-shm`. Copy all three, or checkpoint first;
copying only `memory.db` from a live store can lose the most recent writes.

## 4. What the tables are for

Roles, not a column-by-column dump — the schema of record is
`src/storage/schema.sql`.

| Table | Role |
| --- | --- |
| `edges` | The bitemporal `edge` relation, specialised into real columns. **One row per edge version**: a fact that changed value has one row for each period it held. Carries validity (`valid_from`, `valid_to`), assertion time, confidence, and the retraction columns (`retract_reason`, `retracted_at`, `retracted_by`). |
| `edge_prov` | Provenance for an edge, as a set — one row per source marker, joined on `edge_id`. Not a CSV string, so it is queryable. |
| `facts`, `fact_args`, `fact_prov` | The generic n-ary form for every **non-edge** base predicate: the fact's predicate/arity/confidence, its positional arguments, its provenance. |
| `episodes` | The verbatim source text each fact was extracted from — what `why()` walks back to. Insertion order is load-bearing (episode ids are positional). |
| `escalations` | Conflicts the ingestion policy could not resolve on its own and surfaced to the caller. |
| `rule_batches` | Installed rule programs, in replay order, with their source — so `load` can rebuild derived state. |
| `meta` | Store-level key/value settings, including the engine clock and the schema version key that `load` checks (see §7). |

**Derived facts are not persisted.** Nothing computed by a rule is written
to the store; on load, the base facts and the rule batches are replayed and
every derived relation is recomputed. So the store holds only what was
asserted plus the rules that rederive the rest — and a query straight
against the tables sees base facts, never rule conclusions.

## 5. Sentinels you will meet in the data

Validity is stored as plain integers with two sentinel values:

| Column | Value | Meaning |
| --- | --- | --- |
| `valid_to` | `9223372036854775807` (`i64::MAX`) | Open — the fact is **still true**. |
| `valid_from` | `-9223372036854775808` (`i64::MIN`) | The fact **predates recorded history**; we cannot say when it started. |

Filter on the literal, not on `NULL`. `WHERE valid_to = 9223372036854775807`
is "currently true"; `WHERE valid_to <> 9223372036854775807` is "closed at
some point".

## 6. Useful queries

The store below was built with:

```sh
export LEMMALOG_MCP_PATH=memory.db
lemmalog-cli observe --facts 'svc:billing --owns--> table:invoices
svc:billing --depends_on--> svc:orders
job:invoice_export --status--> running'
lemmalog-cli observe --facts 'job:invoice_export --status--> failed'
```

`status` is a functional relation, so the second `observe` **supersedes**
the first status edge rather than adding a second one.

**What is true right now**

```sh
sqlite3 -header -column memory.db \
  "SELECT subject, predicate, object FROM edges
    WHERE valid_to = 9223372036854775807
    ORDER BY subject, predicate;"
```

```
subject             predicate   object
------------------  ----------  --------------
job:invoice_export  status      failed
svc:billing         depends_on  svc:orders
svc:billing         owns        table:invoices
```

**What stopped being true, when, and why**

```sh
sqlite3 -header -column memory.db \
  "SELECT subject, predicate, object, valid_to, retract_reason FROM edges
    WHERE valid_to <> 9223372036854775807
    ORDER BY valid_to;"
```

```
subject             predicate  object   valid_to    retract_reason
------------------  ---------  -------  ----------  --------------
job:invoice_export  status     running  1789904269  superseded
```

The row stays. That is the point: "true until when, and why not now" is
answerable in SQL.

**Retraction reasons breakdown**

```sh
sqlite3 -header -column memory.db \
  "SELECT COALESCE(retract_reason, '(still open)') AS reason, COUNT(*) AS n
     FROM edges GROUP BY 1 ORDER BY n DESC;"
```

```
reason        n
------------  -
(still open)  3
superseded    1
```

Three reasons exist. `superseded` (an exclusive relation got a new value)
and `world_changed` (it was true until a point) **close** the row and keep
it. `wrong` never appears in the store: a fact we misread was never true,
so its row is deleted and its dependents die with it.

**Provenance for a given fact**

```sh
sqlite3 -header -column memory.db \
  "SELECT e.predicate, e.object, e.valid_to, p.prov
     FROM edges e JOIN edge_prov p ON p.edge_id = e.id
    WHERE e.subject = 'job:invoice_export'
    ORDER BY e.id, p.prov;"
```

```
predicate  object   valid_to             prov
---------  -------  -------------------  ----------
status     running  1789904269           ep1
status     running  1789904269           superseded
status     failed   9223372036854775807  ep2
```

`ep1` / `ep2` are episode ids — join `episodes` on `id` for the verbatim
text a fact came from:

```sh
sqlite3 -header -column memory.db \
  "SELECT id, ts, substr(text, 1, 46) AS text FROM episodes ORDER BY ord;"
```

```
id   ts          text
---  ----------  -------------------------------------
ep1  1789904269  svc:billing --owns--> table:invoices
                 svc:billi

ep2  1789904269  job:invoice_export --status--> failed
```

**Store shape at a glance**

```sh
sqlite3 -header -column memory.db \
  "SELECT 'edges' AS t, COUNT(*) AS n FROM edges
    UNION ALL SELECT 'facts', COUNT(*) FROM facts
    UNION ALL SELECT 'episodes', COUNT(*) FROM episodes
    UNION ALL SELECT 'rule_batches', COUNT(*) FROM rule_batches;"
```

```
t             n
------------  -
edges         4
facts         0
episodes      2
rule_batches  0
```

Four edge rows for three current facts — the superseded one is the fourth.
`facts` is empty because every predicate here is an edge, and
`rule_batches` is empty because no rule program beyond the built-in
defaults was installed.

**Health check**

```sh
sqlite3 memory.db "PRAGMA integrity_check;"   # -> ok
```

## 7. Migration: there is none

`meta` carries a schema version key. `load` accepts only the version the
binary was built for; there is **no migration path**, by design — the store
is a rebuildable projection, not a system of record.

A store written against an older schema fails to load, and the run stops
there:

```console
$ LEMMALOG_MCP_PATH=stale.db lemmalog-cli query --goal 'current(S,R,O)'
lemmalog-cli: "stale.db" exists but could not be loaded (store schema version 1 is older than this binary's schema version 2; no migration path exists — delete this store and rebuild it (save again from a snapshot or a running memory)).
  Refusing to run: continuing with an empty memory would overwrite this store on the next save.
  Move or delete the file, or point LEMMALOG_MCP_PATH elsewhere.
$ echo $?
3
```

**Existence** is what separates a first run from a damaged store, so it is
what the binaries branch on:

| Store file | Behaviour |
| --- | --- |
| absent | start fresh; the first write creates and initialises it |
| present, loads | normal run |
| present, will not load (schema mismatch, corruption, parse failure) | print the error, **exit 3**, write nothing |

A refused run leaves the file byte-identical: the version is checked before
anything is opened for writing, and `save` re-checks it before it opens its
transaction, so a refused `save` writes nothing either. `lemmalog-mcp`
applies the same rule at startup rather than serving an empty memory over a
store it could not read:

```console
$ LEMMALOG_MCP_PATH=stale.db lemmalog-mcp
lemmalog-mcp: "stale.db" exists but could not be loaded: store schema version 1 is older than this binary's schema version 2; no migration path exists — delete this store and rebuild it (save again from a snapshot or a running memory)
  Refusing to start: serving an empty memory would overwrite this store on the first write.
  Move or delete the file, or point LEMMALOG_MCP_PATH elsewhere.
$ echo $?
3
```

To rebuild: move the old file aside, then replay the facts into a new store
(§8) or `save` again from a running memory.

## 8. Worked example: moving a snapshot setup to SQLite

Starting point: a service running with
`LEMMALOG_MCP_PATH=memory.snapshot`.

**0. Build a binary with the feature, and stop the writer.**

```sh
cargo build --release --features mcp,sqlite
```

**1. Dump what is currently true from the snapshot, as line protocol.**
`dump --pred current` prints the derived "true now" relation; the `sed`
turns it back into the assert syntax.

```console
$ LEMMALOG_MCP_PATH=memory.snapshot lemmalog-cli dump --pred current \
    | sed -E 's/^current\(([^,]+), ([^,]+), (.+)\)$/\1 --\2--> \3/' \
    | tee replay.txt
svc:billing --owns--> table:invoices
svc:billing --depends_on--> svc:orders
```

**2. Replay into the new store.** The `.db` extension selects SQLite and
the file is created on the spot.

```console
$ LEMMALOG_MCP_PATH=new.db lemmalog-cli observe --facts "$(cat replay.txt)"
added=2 updated=0 noop=0 escalations=0
```

**3. Verify before cutting over.**

```console
$ LEMMALOG_MCP_PATH=new.db lemmalog-cli query --goal 'current(S, "owns", O)'
S=svc:billing, O=table:invoices
$ sqlite3 -header -column new.db "SELECT COUNT(*) AS edges FROM edges;"
edges
-----
2
```

**4. Cut over.** Point `LEMMALOG_MCP_PATH` at the `.db` for every process
(for the MCP server, the `--env LEMMALOG_MCP_PATH=...` in its registration)
and restart them.

**5. Keep the snapshot.** It is the only copy of the pre-cutover state, and
there is no way back from the `.db` other than another replay.

What a replay does **not** carry over: closed (historical) edge versions,
the original episode text, and per-fact assertion timestamps. The new store
starts from "what is true now", re-asserted as of the replay instant. If
that history matters, keep the snapshot file as the archive of it.
