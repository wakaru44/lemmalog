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

To rebuild: move the old file aside, then `save` again from a running
memory. `convert` (§9) cannot help here — it has to load the source, and
this source is exactly what will not load.

## 9. Moving a store between backends: `convert`

```sh
lemmalog-cli convert --from <path> --to <path> [--force]
```

`convert` is the only subcommand that names its paths itself instead of
reading `LEMMALOG_MCP_PATH`, because it is the only one that touches two
stores. **Each side picks its own backend from its own extension** — the
same rule as §1 — so all four directions work with one command:

| From | To | What it is |
| --- | --- | --- |
| `.snapshot` | `.db` | migrate an existing snapshot setup to SQLite |
| `.db` | `.tsv` | export a SQLite store to a git-diffable text file (§11) |
| `.tsv` | `.db` | import that text file back into SQLite |
| `.db` | `.db` | copy/compact a store |
| `.snapshot` | `.snapshot` | copy a snapshot (works in a binary built without `sqlite`) |

It is `load` then `save`: the **whole memory** moves, not the "what is
true now" projection a dump-and-replay moves. Closed edge versions, the
retraction columns, the original valid/assertion timestamps, episode text
and the assertion provenance all survive, because they are all things both
backends persist.

### Migrating a snapshot to SQLite

```console
$ lemmalog-cli convert --from memory.snapshot --to memory.db
converted memory.snapshot -> memory.db
edges=3 other_base_facts=4 episodes=1 escalations=0 rule_batches=2
```

The second line is there so the counts can be eyeballed against the source
before anything is cut over. `other_base_facts` is every **non-edge** base
predicate (§4's `facts` table); derived relations are not counted because
neither backend persists them — they are recomputed on load.

History is in the destination, not just the current values:

```console
$ sqlite3 -header -column memory.db \
    "SELECT subject, predicate, object, valid_to, retract_reason, retracted_by, fact_class
       FROM edges ORDER BY id;"
subject             predicate   object          valid_to             retract_reason  retracted_by  fact_class
------------------  ----------  --------------  -------------------  --------------  ------------  ----------
svc:billing         owns        table:invoices  9223372036854775807                                agent
svc:billing         depends_on  svc:orders      9223372036854775807                                agent
job:invoice_export  status      running         1789937527           world_changed   agent-7       agent
```

The third row is a fact that **stopped** being true. A replay would not
have carried it at all.

### Cut over

Point `LEMMALOG_MCP_PATH` at the `.db` for every process (for the MCP
server, the `--env LEMMALOG_MCP_PATH=...` in its registration, or §10's
`.mcp.json`) and restart them. Keep the source file: nothing deletes it,
and it is the archive of the pre-cutover state.

### It refuses rather than guess

| Situation | Behaviour |
| --- | --- |
| `--to` exists | exit 2, nothing written — pass `--force` to overwrite |
| `--from` missing | exit 2, nothing written |
| `--from` exists but will not load | exit 3, nothing written |
| either side names `.db` in a binary built without `sqlite` | exit 2, nothing written |

```console
$ lemmalog-cli convert --from memory.snapshot --to memory.db
lemmalog-cli: --to "memory.db" already exists.
  Refusing to overwrite it: convert writes the destination whole.
  Move it aside, pick another path, or pass --force.
$ echo $?
2
```

`convert` writes the destination **whole**, so an unguarded `--to` is the
store-wipe of §8 wearing a different hat. `--force` exists for the case
where overwriting is the intent.

```console
$ lemmalog-cli convert --from gone.snapshot --to new.db
lemmalog-cli: --from "gone.snapshot" does not exist; nothing to convert
$ echo $?
2
```

A missing source is an **error**, not "start fresh" — unlike every other
subcommand, where absence legitimately means a first run. Converting from
nothing would produce an empty destination and exit 0, which looks exactly
like a successful migration and is the failure nobody checks.

```console
$ lemmalog-cli convert --from broken.snapshot --to new.db
lemmalog-cli: --from "broken.snapshot" exists but could not be loaded (not a lemmalog snapshot).
  Refusing to run: nothing was written to "new.db".
$ echo $?
3
$ ls new.db
ls: new.db: No such file or directory
```

Same exit code and same rule as §8: present-but-unreadable stops the run.

### The old way: replay, and what it costs

Before `convert` the only migration was a replay — dump the derived
`current` relation, turn it back into assert syntax, and observe it into
the new store:

```console
$ LEMMALOG_MCP_PATH=memory.snapshot lemmalog-cli dump --pred current \
    | sed -E 's/^current\(([^,]+), ([^,]+), (.+)\)$/\1 --\2--> \3/'
svc:billing --owns--> table:invoices
svc:billing --depends_on--> svc:orders
```

Two lines out of a three-edge store. What a replay drops:

- **every closed edge version** — the `status running` row above, and with
  it the whole "true until when, and why not now" answer §7 is built on;
- `retract_reason`, `retracted_at`, `retracted_by`;
- the original `valid_from` / `asserted_at` — re-asserted facts are
  stamped with the replay instant;
- the original assertion provenance — the new rows carry the HEAD, branch
  and class the **replay** ran under, not the ones that first asserted them;
- the verbatim episode text `why()` walks back to;
- escalations, and any non-edge base fact (`dump --pred current` only
  prints edges).

For a bitemporal store that is most of the value. **Use `convert` to
migrate.**

The replay is still a real tool — for **deliberately discarding history**.
It is how you take a store that has accumulated years of superseded
versions, retracted claims and stale episode text and start a clean one
holding only what is true today, re-stamped with the commit that made that
decision. Wanting the history gone is a legitimate reason to run it; being
in a hurry is not.

## 10. Per-repo configuration: a committed `.mcp.json`

`LEMMALOG_MCP_PATH`, `LEMMALOG_ONTOLOGY` and `LEMMALOG_REPO` are all read
from the environment, which means they can be set **per project** in a
`.mcp.json` committed at the repository root — one store and one vocabulary
per project, shared by everyone who checks the repo out.

```json
{
  "mcpServers": {
    "lemmalog": {
      "command": "lemmalog-mcp",
      "args": [],
      "env": {
        "LEMMALOG_MCP_PATH": ".lemmalog/memory.db",
        "LEMMALOG_ONTOLOGY": "lemmalog.yaml",
        "LEMMALOG_REPO": "."
      }
    }
  }
}
```

**Keep the paths relative.** They resolve against the directory the client
launches the server in, which for a project-scoped `.mcp.json` is the
project root. An absolute path committed to a shared file is somebody's
home directory, and it is wrong for every other checkout — or, worse,
right enough to point two projects at one store.

Each variable does something different, and all three are worth setting:

| Variable | What it pins |
| --- | --- |
| `LEMMALOG_MCP_PATH` | **Which store.** A path inside the project gives the project its own memory instead of one global store shared by everything. |
| `LEMMALOG_ONTOLOGY` | **Which vocabulary.** A committed `lemmalog.yaml` is this project's relation set; unknown relations are rejected at write time rather than silently inventing meaning (see [`ontology.md`](ontology.md)). |
| `LEMMALOG_REPO` | **What writes are stamped with.** `.` is this checkout, so `asserted_at_sha` / `asserted_on_branch` (§6) name the commit the fact was actually asserted at. |

The CLI reads exactly the same variables, so a script run from the project
root gets the same store and the same vocabulary as the MCP server:

```console
$ LEMMALOG_MCP_PATH=.lemmalog/memory.db LEMMALOG_ONTOLOGY=lemmalog.yaml LEMMALOG_REPO=. \
    lemmalog-cli observe --facts 'svc:x --owns--> table:y'
added=1 updated=0 noop=0 escalations=0
asserted_at_sha=7e0121f79105cbfdb025007a600526ba19a886ee asserted_on_branch=w44/feat/alt_storage fact_class=agent
```

An undeclared relation is refused by the project's own vocabulary, not by
a global one:

```console
$ LEMMALOG_MCP_PATH=.lemmalog/memory.db LEMMALOG_ONTOLOGY=lemmalog.yaml LEMMALOG_REPO=. \
    lemmalog-cli observe --facts 'svc:x --frobnicates--> table:y'
added=0 updated=0 noop=0 escalations=0 rejected=1
asserted_at_sha=7e0121f79105cbfdb025007a600526ba19a886ee asserted_on_branch=w44/feat/alt_storage fact_class=agent
rejected: svc:x --frobnicates--> table:y (unknown relation `frobnicates`)
```

Two decisions to make deliberately: whether `.lemmalog/` is committed or
`.gitignore`d (committing it is §11), and whether `command` is a bare
`lemmalog-mcp` on everyone's `PATH` or an absolute path to a build. The
`.mcp.json` above is not a secret — it holds no credentials — but it does
decide where every agent on the project writes.

## 11. Sharing a store through git: the TSV round trip

The snapshot backend is a tab-separated text file, so any non-SQLite
extension gives you a text export. `.tsv` is the honest name for it:

```console
$ lemmalog-cli convert --from memory.db --to memory.tsv
converted memory.db -> memory.tsv
edges=3 other_base_facts=4 episodes=1 escalations=0 rule_batches=2

$ head -4 memory.tsv
LEMMALOG1
NOW	1789937527
RULES	
RULEB	b1	reports_to(X,\sY)\s:-\scurrent(X,\s"manager",\sY).
```

and back again:

```console
$ lemmalog-cli convert --from memory.tsv --to rebuilt.db
converted memory.tsv -> rebuilt.db
edges=3 other_base_facts=4 episodes=1 escalations=0 rule_batches=2
```

The round trip is lossless in both directions — the exported text and a
snapshot written directly hold the same line set:

```console
$ diff <(sort memory.snapshot) <(sort memory.tsv) && echo "IDENTICAL (line-set)"
IDENTICAL (line-set)
```

**What it is for.** A `.db` is a binary blob: `git diff` says "Binary files
differ" and review stops there. The TSV is reviewable — you can see in a
merge request that one fact was added and which one — and it is greppable
with `cut`, `grep` and `sort` without a SQLite client. That makes it the
form to commit when a store is a shared artifact of the project rather than
one agent's scratch memory: the `.db` for working, the `.tsv` for the
history and the review.

**The caveat, and it is the same one as §3.** The TSV is rewritten
**whole** on every mutation, and line order follows an internal hash map,
so a one-fact change does not produce a one-line diff:

```console
$ cp memory.tsv before.tsv
$ LEMMALOG_MCP_PATH=memory.tsv lemmalog-cli observe --facts 'svc:orders --owns--> table:orders'
added=1 updated=0 noop=0 escalations=0
asserted_at_sha=7e0121f79105cbfdb025007a600526ba19a886ee asserted_on_branch=w44/feat/alt_storage fact_class=agent
$ diff before.tsv memory.tsv
2c2
< NOW	1789937527
---
> NOW	1789937624
6a7,11
> EP	ep2	1789937624		svc:orders\s--owns-->\stable:orders
> EPCTX	ep2	7e0121f79105cbfdb025007a600526ba19a886ee	w44/feat/alt_storage	agent
> FACT	edge_evidence	1		s:svc:billing s:owns s:table:invoices s:ep1
> FACT	edge_evidence	1		s:svc:billing s:depends_on s:svc:orders s:ep1
> FACT	edge_evidence	1		s:job:invoice_export s:status s:running s:ep1
9a15
> FACT	edge	0.9	ep2	s:svc:orders s:owns s:table:orders i:1789937624 i:9223372036854775807 i:1789937624
11,13d16
< FACT	edge_evidence	1		s:svc:billing s:owns s:table:invoices s:ep1
< FACT	edge_evidence	1		s:svc:billing s:depends_on s:svc:orders s:ep1
< FACT	edge_evidence	1		s:job:invoice_export s:status s:running s:ep1
```

One new fact — two lines genuinely added (`EP ep2`, the new `FACT edge`)
plus its episode context — and the diff also moves three unrelated
`edge_evidence` lines from one end of the file to the other. It is
**readable** — you can find the new records — but it is not minimal, and
git will conflict on hunks that contain no real disagreement.

So the TSV suits a **single writer**: one process (or one person) mutates
the store and commits it, everyone else reads, and facts from others are
handed to the writer rather than committed in parallel. It does not suit
concurrent editing — two branches that both touched the store will conflict
on lines neither of them meant to change, and resolving that conflict by
hand means hand-editing bitemporal records. If more than one process needs
to write, use the `.db` (§3) and export the TSV from it as a read-only
artifact.
