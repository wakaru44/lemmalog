# Why this repo

Technical source material for a talk on agent memory as a deductive
database. Everything here is verified against this tree and against a real fact
store built with it; no claim is rhetorical.

---

## 0. The problem, stated once

We are reverse-engineering a large legacy monolith: thousands of
methods, only a small fraction of them documented past machine-generated
scaffolding. The work runs for months, across many agent sessions, many
branches, several humans.

Every one of those sessions learns things. Almost all of it is lost.

What survives has to be more than notes, because the codebase moves
while we document it. A note that was true in March is not marked false
in September — it is just quietly wrong, and nothing tells you which of
the conclusions you built on it are now wrong too.

That last sentence is the whole talk.

---

## 1. Datalog and Prolog: what they actually are

**Prolog** (1972) is logic as a programming language. You state facts
and rules; the engine searches for proofs.

```prolog
parent(anna, bob).
parent(bob, carol).
ancestor(X, Y) :- parent(X, Y).
ancestor(X, Y) :- parent(X, Z), ancestor(Z, Y).
```

`?- ancestor(anna, carol).` → `true`. You never wrote a traversal. You
described what "ancestor" *means* and the engine did the walking.

Prolog resolves **top-down** (SLD resolution), clause order matters, and
`cut` lets you prune the search by hand. It is Turing-complete, which
means it can also loop forever.

**Datalog** is Prolog with the dangerous parts removed:

| | Prolog | Datalog |
|---|---|---|
| Function symbols | yes | **no** |
| Evaluation | top-down, order-sensitive | **bottom-up, order-insensitive** |
| Termination | not guaranteed | **guaranteed** |
| Semantics | procedural in practice | **pure sets** |
| Negation | `\+`, unsafe | stratified, safe |

Those restrictions buy three properties that matter enormously here:

1. **It always terminates.** Safe to hand an LLM as a query language.
2. **It has no side effects.** A query cannot corrupt the store.
3. **It is a database, not a program.** Rules define *views*, recomputed
   from base facts. There is no state to get out of sync.

Datalog is not obscure. It is the query layer under Souffle (static
analysis at Oracle/Meta scale), Rust's Polonius borrow checker,
LogicBlox, and RelationalAI.

---

## 2. Why this shape fits facts about a legacy codebase

Reverse-engineering produces two kinds of statement:

- **Observations** — "`module:billing` writes `table:invoices`."
- **Rules** — "if a module writes a table, and that table represents an
  entity, that module touches that entity's lifecycle."

Every other memory system stores only the first kind. Datalog stores
both, and the second is where understanding actually lives.

```prolog
touches_entity(M, E) :-
    current(M, "writes_table", T),
    current(T, "represents", E).
```

Assert 50 `writes_table` facts and 30 `represents` facts; get the module
to entity map **for free**, forever, kept correct as facts change. You
did not build a map. You said what a map *is*.

Three consequences:

**Consistency by construction.** A derived fact cannot disagree with its
inputs. There is no cache to invalidate because there is no cache.

**Fewer assertions.** Agents assert what they *saw in the code* and stop.
They do not assert conclusions, so they cannot assert wrong conclusions.

**Questions you cannot otherwise ask.** "What did we conclude, and from
what?" is a reachability query — two rules. In SQL or Cypher it is an
application.

And the queries are recursive by nature: *is this claim transitively
supported? what depends on this decision? which evidence feeds this
summary?* Recursion is the native shape of Datalog and an awkward guest
everywhere else.

---

## 3. What vector databases cannot do

Embeddings retrieve by **similarity**. That is a genuinely useful
operation and it is not the operation we need.

- **Similar is not true.** A retrieved chunk that contradicts another
  retrieved chunk produces no error. Both come back, ranked.
- **No invalidation.** Delete a wrong fact and everything it implied
  stays in the index, still similar, still retrievable, now wrong.
- **No dependency.** "What else breaks if this is false?" is not
  expressible. Cosine distance does not know what *because* means.
- **No provenance.** You get a chunk and a score, not a proof.

Vector search answers *what does this resemble*. We are asking *what
follows from this*. Different question, different machine.

Their real place is the ingestion boundary: finding candidate evidence
for a human or agent to verify, before it becomes a fact. This engine
keeps a pluggable `Embedder` trait for exactly that seeding role — and
nothing downstream of it.

---

## 4. What Neo4j cannot do

Property graphs are the closer miss, which makes the gap worth naming
precisely. Neo4j is genuinely good: mature Cypher, variable-length
traversal, vector indexes, real concurrency.

It has no derivation and no truth maintenance.

```cypher
MATCH ()-[r:WRITES_TABLE]->() WHERE ... DELETE r
```

That deletes one edge. Every conclusion you drew from it is still
sitting in the graph, indistinguishable from a sound one. Nothing is
flagged, nothing is recomputed, nothing tells you what just became
unsupported. The graph is now confidently wrong and looks exactly like
it did when it was right.

You *can* build justification tracking in Cypher —
`(:Fact)-[:SUPPORTED_BY]->(:Fact)` plus a cascade procedure. That is
reimplementing a deductive database by hand, in a language that does not
want to be one, and maintaining it forever.

Neo4j also has no ontology enforcement worth the name (schemaless by
default, plus uniqueness constraints), and it is the wrong container for
large documents — those belong in the repo, addressed by content hash.

Scale, honestly: our target is ~200k facts. That is small. Neo4j's
traversal performance is not the constraint we have, so we would be
paying its operational cost for a benefit we cannot spend.

---

## 5. The dual timeline

This is the part people skip and then regret.

A fact about a codebase has **two independent times**:

- **Valid time** — when the fact was true *of the code*.
- **Assertion time** — when *we believed* it.

Collapse them and two completely different events become
indistinguishable:

> *"`module:billing` writes `table:invoices`" is no longer in the store.*

Did the code change, or were we wrong?

If we were wrong, everything built on that fact is poisoned and must
die. If the code changed, everything built on it was **correct for its
period** and merely needs rechecking against the new code. Same
disappearance. Opposite consequences.

One clock cannot express the difference. This is not a refinement; it is
the difference between a memory that decays into noise and one that
accumulates.

Bitemporal modelling is old and settled — Snodgrass formalized it in the
1990s, every serious temporal database implements it. What is new is
that agent memory needs it and almost no agent memory system has it.

A third state falls out of having two clocks, and it is the most
valuable output of the whole system: **suspect**. A fact whose support
closed in valid time is not true and not false — it is *unverified
against current reality*. That set is a re-verification queue. On a
months-long archaeology project, knowing precisely what to re-check is
worth more than any individual fact in the store.

And a bound at the other end: much of this monolith predates its own git
history. `valid_from` therefore needs an open-at-the-left marker, `-inf`
with `bounded_by`, so "introduced at commit X" stays distinguishable
from "older than anything we can see."

---

## 6. Retraction is cheap here, and only here

The engine annotates every fact with a **provenance semiring** (Green et
al., PODS 2007): confidence composes as a product across a join, merges
as a max across independent derivations, and provenance unions.

```rust
// src/eval.rs:42  — conjunction
conf: (self.conf * other.conf).clamp(0.0, 1.0),

// src/eval.rs:114 — same fact, two derivations
existing.ann.conf = existing.ann.conf.max(f.ann.conf);
```

Every derived fact therefore already carries *what it came from*. Three
operations become one-liners instead of projects:

| Operation | How |
|---|---|
| `why(F)` | walk the proof tree — already recorded |
| `retract(F)` | drop it, re-derive, report what died |
| `what_if(F)` | derive with and without, diff |

`lemmalog_retract` *returns the list of conclusions that died*. In any
store without derivation, that list does not exist and cannot be
computed, because nothing ever recorded that those conclusions depended
on anything.

**And at our scale, this is nearly free.** 200k facts is small enough
that full re-materialization runs in well under a second. The expensive
engineering — incremental truth maintenance, dependency bookkeeping — is
a premature optimization. We need cheap recompute plus provenance, and
we have both.

That is the punchline: *invalidation is the hard problem in every other
memory architecture, and the easy case in this one.*

---

## 7. How our proposal works

A fork of [lemmalog](https://github.com/JordyZomer/lemmalog) (MIT) — a
Rust Datalog engine, ~6.5k lines, with seminaive evaluation, magic-set
demand transformation, bitemporal relations and semiring annotations
already in place. We add what a shared, months-long archaeology project
needs and upstream does not owe us.

**Kept from upstream:** the evaluator (`eval.rs`, `magic.rs`), the
bitemporal data model, the MCP tool surface, the change-feed epochs that
let an agent resync after a context reset.

**What we change:**

**Storage.** Upstream persists one snapshot file, rewritten whole after
every mutation. For a git-tracked team store this is fatal: every commit
is a full-file diff, every concurrent write silently erases the other's
facts, and readers see nothing new until they restart their session. We
move to SQLite via `rusqlite`, `STRICT` tables, portable SQL. Every one of
those pathologies is a property of the file, not of the model.

One limit is worth separating out, because it looks like a storage
problem and is not. Values are clipped at 8 words and 60 characters and
non-ASCII is dropped, so `café` never becomes a fact. That is a
**parser guard** (`src/agent.rs:107`), deliberately placed to stop a
reasoning model leaking prose and deliberation into the store. SQLite
does not fix it and should not. Whether to relax it for a
non-English codebase is a separate argument with a real case on both
sides.

**Three retraction reasons** instead of one:

| Reason | Closes | Dependents |
|---|---|---|
| `wrong` | assertion time — never valid | die |
| `world_changed` | valid time, at a commit | true before it, **suspect** after |
| `superseded` | both, chained | re-derive |

**`suspect` as a rule, not a column** — derived from supports closed in
valid time, so it recomputes for free and cannot drift.

**Provenance anchored to the repo.** Every fact carries
`asserted_at_sha` and `asserted_on_branch`. Deferred but cheap once
those exist: a pickaxe backfill (`git log -S --reverse`) that mechanically
recovers when a fact *became* true — no LLM, just git.

**A real ontology.** One `ontology.yaml` — relation domains, ranges,
cardinality — read by the linter, enforced by the server at write time,
and rendered into the agent prompt. Today that vocabulary is split
across prose, a lint script and a pre-commit hook, so any agent that
skips the hook is unconstrained. Unknown relations get rejected; unknown
entity names get flagged, because the vocabulary of a legacy system is
genuinely open and blocking on it would stall the work.

**Fact classes.** Machine-derived (a static-analysis pass, an AST
indexer — regenerated per commit, never hand-retracted), agent-asserted (evidence and confidence
required), human (authoritative, low volume). Rules bridge them;
confidence flows through the semiring. Re-running the machine tier at a
new commit *is* the change-detection engine — diff two runs, and the
agent facts that depended on what moved go suspect automatically.

---

## 8. The shape of the claim

Not "graphs are good for memory." That is agreed and insufficient.

**A project's accumulated understanding is a deductive database, and the
operation that decides whether it accumulates or decays is retraction.**

Storage systems make retraction expensive or impossible, so in practice
nobody retracts, so memory rots. A Datalog engine with provenance makes
retraction a query — and a second clock makes it *meaningful*, because
only then can a system tell "we were wrong" from "the world moved."

Everything else here is plumbing.
