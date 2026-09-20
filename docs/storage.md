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

The other operator-facing knob, `LEMMALOG_REPO`, decides what each write is
stamped with rather than where it goes; it is §6.

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
| `edges` | The bitemporal `edge` relation, specialised into real columns. **One row per edge version**: a fact that changed value has one row for each period it held. Carries validity (`valid_from`, `valid_to`), assertion time, confidence, the retraction columns (`retract_reason`, `retracted_at`, `retracted_by`), and the assertion-provenance columns (`asserted_at_sha`, `asserted_on_branch`, `fact_class`, `bounded_by` — §6). |
| `edge_prov` | Provenance for an edge, as a set — one row per source marker, joined on `edge_id`. Not a CSV string, so it is queryable. |
| `facts`, `fact_args`, `fact_prov` | The generic n-ary form for every **non-edge** base predicate: the fact's predicate/arity/confidence, its positional arguments, its provenance. |
| `episodes` | The verbatim source text each fact was extracted from — what `why()` walks back to — plus the commit, branch and class that ingestion ran under (`sha`, `branch`, `fact_class`). Insertion order is load-bearing (episode ids are positional). |
| `escalations` | Conflicts the ingestion policy could not resolve on its own and surfaced to the caller. |
| `rule_batches` | Installed rule programs, in replay order, with their source — so `load` can rebuild derived state. |
| `meta` | Store-level key/value settings: the engine clock (`now`), any extra rule source (`extra_rules`), and the `schema_version` key that `load` checks (see §8). |

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

## 6. Assertion provenance: which commit claimed this

Every ingestion also records **where it was asserted from**. The rationale
is `docs/adr/0003-defer-dag-valid-time.md`: git is a DAG, bitemporal valid
time is a line, and rather than decide that now we buy the columns that let
us decide later.

### The two knobs

| Knob | Set on | Effect |
| --- | --- | --- |
| `LEMMALOG_REPO` | environment, both binaries | The working tree whose HEAD is recorded. Unset means **the current working directory**. |
| `--fact-class` (CLI) / `fact_class` (MCP `lemmalog_observe`) | per ingestion | How authoritative the batch is: `machine`, `agent` (default) or `human`. |

`LEMMALOG_REPO` deliberately does **not** default to the directory holding
the store file. The agent normally runs inside the codebase it is
documenting while the store lives somewhere else entirely, so the two are
named apart.

Resolution is `git rev-parse`, not a hand-rolled `.git/HEAD` reader —
worktrees, packed refs and detached HEAD are all already correct in
`rev-parse` and all wrong in anything you write yourself. Note the
consequence: **`rev-parse` walks up**, so pointing `LEMMALOG_REPO` at a
subdirectory silently resolves the repository that encloses it.

```console
$ LEMMALOG_REPO=./app/sub/deep LEMMALOG_MCP_PATH=walkup.db \
    lemmalog-cli observe --facts 'svc:orders --owns--> table:orders'
added=1 updated=0 noop=0 escalations=0
asserted_at_sha=b699e1ce1e4563f445db593a3c3f812b5f8ff242 asserted_on_branch=release/2026.09 fact_class=agent
```

That second line is the echo of what was recorded, printed **only when
there is something to say**. A run with no sha, no branch and the default
class prints exactly the summary line it always printed.

### Provenance belongs to the ingestion, not to the triple

An episode is one ingestion call. Every fact in that call shares one
commit, one branch and one class, so `episodes` is where they live and
where `load` reads them back from. `save` denormalises the same three
values onto each `edges` row so that `WHERE asserted_on_branch = ?` needs
no join. The episode is the source of truth; the edge columns are the
index.

### Outside a checkout: silence, not failure

Not a repo, no `git` on `PATH`, a repo with no commits yet — all the same
answer: `sha` and `branch` stay `NULL`, `fact_class` keeps its default, and
nothing is printed or warned about. A store used outside a checkout behaves
exactly as it did before these columns existed.

```console
$ cd /some/directory/that/is/not/a/repo
$ LEMMALOG_MCP_PATH=norepo.db lemmalog-cli observe --facts 'svc:billing --owns--> table:invoices'
added=1 updated=0 noop=0 escalations=0
$ sqlite3 -header -column norepo.db \
    "SELECT subject, asserted_at_sha IS NULL AS sha_null,
            asserted_on_branch IS NULL AS branch_null, fact_class FROM edges;"
subject      sha_null  branch_null  fact_class
-----------  --------  -----------  ----------
svc:billing  1         1            agent
```

### Detached HEAD: the sha, and no branch

```console
$ git -C ./app checkout --detach HEAD
$ LEMMALOG_MCP_PATH=detached.db lemmalog-cli observe --facts 'svc:orders --owns--> table:orders'
added=1 updated=0 noop=0 escalations=0
asserted_at_sha=b699e1ce1e4563f445db593a3c3f812b5f8ff242 fact_class=agent
$ sqlite3 -header -column detached.db \
    "SELECT subject, substr(asserted_at_sha,1,8) AS sha, asserted_on_branch FROM edges;"
subject     sha       asserted_on_branch
----------  --------  ------------------
svc:orders  b699e1ce
```

This is a choice, not an omission. `git rev-parse --abbrev-ref HEAD` on a
detached HEAD answers the literal string `HEAD`, which is not a branch name
and which no `git checkout` can ever match. Storing it — or any invented
stand-in like `detached` or the nearest tag — would put a value in
`asserted_on_branch` that queries would happily match and that means
nothing. The sha is recorded, and the sha is what identifies the revision.

### `fact_class`: three values, no silent demotion

| Value | Means |
| --- | --- |
| `machine` | Regenerated by tooling. Never hand-retracted — it is rebuilt from its generator. |
| `agent` | Asserted by an agent with evidence. The default. |
| `human` | A human said so. Authoritative; outranks the other two. |

An unrecognised value is an error naming the valid set, and the run stops
before anything is ingested:

```console
$ lemmalog-cli observe --fact-class robot --facts 'svc:a --owns--> table:b'
lemmalog-cli: unknown fact_class "robot" — expected machine, agent or human
$ echo $?
2
```

Quietly demoting an unknown class to `agent` would put a wrong authority
level on real facts, which is worse than refusing the write. The lifecycle
difference between `machine` and the other two is in
[`ontology.md`](ontology.md).

### `bounded_by` is a column, not a feature yet

`bounded_by` round-trips through save/load and pairs with
`valid_from = -9223372036854775808` ("predates recorded history") to say
how far back the search that produced the fact could actually see —
distinguishing "always been true" from "true as far as we could look".

**Nothing populates it.** The `git log -S --reverse` pickaxe backfill that
would is deferred (ADR 3). In every store written today the column is
`NULL` on every row:

```console
$ sqlite3 -header -column memory.db \
    "SELECT COUNT(*) AS rows_with_bounded_by FROM edges WHERE bounded_by IS NOT NULL;"
rows_with_bounded_by
--------------------
0
```

Do not build a query that depends on it.

### Cost, and why the MCP server re-resolves

`lemmalog-mcp` resolves HEAD **per ingestion**, not once at startup. The
server is long-lived and HEAD moves under it during a session; a sha
resolved at boot would label every later fact with a commit it was not
asserted at.

The price is two `git rev-parse` forks per ingestion — ~16 ms on the
machine these examples were run on (50 rounds of both calls took 0.825 s
wall). It is paid on writes only, never on a read.

## 7. Useful queries

The store below was built with:

```sh
export LEMMALOG_MCP_PATH=memory.db
export LEMMALOG_REPO=./app            # a checkout, currently on `main`
lemmalog-cli observe --facts 'svc:billing --owns--> table:invoices
svc:billing --depends_on--> svc:orders
job:invoice_export --status--> running'

git -C ./app checkout release/2026.09
lemmalog-cli observe --facts 'job:invoice_export --status--> failed'
lemmalog-cli observe --fact-class machine --facts 'svc:billing --calls--> svc:ledger'
```

`status` is a functional relation, so the second `observe` **supersedes**
the first status edge rather than adding a second one. The third is tagged
`machine` to stand in for a fact some generator produces.

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
svc:billing         calls       svc:ledger
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
job:invoice_export  status     running  1789923728  superseded
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
(still open)  4
superseded    1
```

Three reasons exist. `superseded` (an exclusive relation got a new value)
and `world_changed` (it was true until a point) **close** the row and keep
it. `wrong` never appears in the store: a fact we misread was never true,
so its row is deleted and its dependents die with it.

**What did branch X claim?**

```sh
sqlite3 -header -column memory.db \
  "SELECT subject, predicate, object, substr(asserted_at_sha, 1, 8) AS sha
     FROM edges WHERE asserted_on_branch = 'release/2026.09'
    ORDER BY id;"
```

```
subject             predicate  object      sha
------------------  ---------  ----------  --------
job:invoice_export  status     failed      b699e1ce
svc:billing         calls      svc:ledger  b699e1ce
```

Note what is *absent*: the closed `status running` edge is not here,
because it was asserted from `main`. Branch attribution survives
supersession — each edge version keeps the branch that wrote it.

**Which facts came from tooling rather than an agent?**

```sh
sqlite3 -header -column memory.db \
  "SELECT fact_class, COUNT(*) AS n FROM edges GROUP BY fact_class ORDER BY n DESC;"
```

```
fact_class  n
----------  -
agent       4
machine     1
```

```sh
sqlite3 -header -column memory.db \
  "SELECT subject, predicate, object FROM edges
    WHERE fact_class = 'machine' AND valid_to = 9223372036854775807;"
```

```
subject      predicate  object
-----------  ---------  ----------
svc:billing  calls      svc:ledger
```

That second query is the one to run before a cleanup: machine-class facts
are the ones to regenerate rather than retract by hand.

**Provenance per ingestion (the authoritative copy)**

```sh
sqlite3 -header -column memory.db \
  "SELECT id, substr(sha, 1, 8) AS sha, branch, fact_class FROM episodes ORDER BY ord;"
```

```
id   sha       branch           fact_class
---  --------  ---------------  ----------
ep1  19aea2b0  main             agent
ep2  b699e1ce  release/2026.09  agent
ep3  b699e1ce  release/2026.09  machine
```

Three ingestion calls, three episodes, one `(sha, branch, class)` triple
each. The `edges` columns are the same values spread across the five rows
those calls produced.

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
status     running  1789923728           ep1
status     running  1789923728           superseded
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
ep1  1789923716  svc:billing --owns--> table:invoices
                 svc:billi

ep2  1789923728  job:invoice_export --status--> failed

ep3  1789923728  svc:billing --calls--> svc:ledger
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
edges         5
facts         0
episodes      3
rule_batches  0
```

Five edge rows for four current facts — the superseded one is the fifth.
`facts` is empty because every predicate here is an edge, and
`rule_batches` is empty because no rule program beyond the built-in
defaults was installed.

**Health check**

```sh
sqlite3 memory.db "PRAGMA integrity_check;"   # -> ok
```

## 8. Migration: there is none

`meta` carries a schema version key — **3** as of the assertion-provenance
columns. `load` accepts only the version the binary was built for; there is
**no migration path**, by design — the store is a rebuildable projection,
not a system of record.

A store written against an older schema fails to load, and the run stops
there:

```console
$ LEMMALOG_MCP_PATH=stale.db lemmalog-cli query --goal 'current(S,R,O)'
lemmalog-cli: "stale.db" exists but could not be loaded (store schema version 2 is older than this binary's schema version 3; no migration path exists — delete this store and rebuild it (save again from a snapshot or a running memory)).
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
lemmalog-mcp: "stale.db" exists but could not be loaded: store schema version 2 is older than this binary's schema version 3; no migration path exists — delete this store and rebuild it (save again from a snapshot or a running memory)
  Refusing to start: serving an empty memory would overwrite this store on the first write.
  Move or delete the file, or point LEMMALOG_MCP_PATH elsewhere.
$ echo $?
3
```

To rebuild: move the old file aside, then replay the facts into a new store
(§9) or `save` again from a running memory.

## 9. Worked example: moving a snapshot setup to SQLite

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
asserted_at_sha=b699e1ce1e4563f445db593a3c3f812b5f8ff242 asserted_on_branch=release/2026.09 fact_class=agent
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
the original episode text, and per-fact assertion timestamps. Nor the
original provenance — as the console output in step 2 shows, the replayed
facts are stamped with the HEAD the *replay* ran on, not the one that first
asserted them. The new store starts from "what is true now", re-asserted as
of the replay instant. If that history matters, keep the snapshot file as
the archive of it.
