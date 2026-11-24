# The store cannot be merged across branches

**Raised:** 2026-09-21 · **Status:** problem statement only. No solution chosen.
**Next session:** study and decide. Do not implement from this file.

## The problem

The fact store is a git-tracked file shared by a team working on branches. Two
people asserting facts on two branches produce two divergent stores that git
cannot reconcile — and the failure is silent rather than loud.

Concretely: episode ids are **positional** (`ep{episodes.len()}`), so both
branches independently allocate `ep122` to different content. Every fact's
provenance is a *string* naming an episode id. Merge the two files and you get
two `ep122` records; every provenance reference to it is now ambiguous. **The
merged store loads without error and `why()` returns a confident, wrong
answer.** Nothing fails.

A positional counter is a single-writer assumption. Git is distributed. Those
are incompatible, and no branching discipline patches over it.

## Why it is not just an id problem

Even with unique ids, the store still conflicts on every commit, because
`AgentMemory::save` iterates a `HashMap`, so serialisation order is
nondeterministic. Observed during the convert round-trip: identical line *set*,
different relation-block *order*. So a no-op save rewrites the file in a new
order and every commit is a full-file diff.

Fix only the ids and you still conflict constantly. Fix only the ordering and
you still corrupt silently. **Both are required, and they are independent.**

## Gotchas any solution must survive

- **Provenance is by id, as an opaque string.** Renaming or reallocating an
  episode id silently invalidates every fact that cites it. Nothing validates
  that a prov string resolves.
- **`episode_counter` is restored from `episodes.len()`**, and `episodes.ord`
  (insertion order) is load-bearing for id stability. Changing the id scheme
  touches all three.
- **Supersession rewrites an existing row's `valid_to`.** That is a line
  *change*, not an append — so an append-only format does not make every merge
  clean. Two branches closing the same edge differently is a *genuine*
  disagreement and should conflict. That is desirable, not a bug to design out.
- **Retraction markers ride inside the provenance set** (`retract:`,
  `retracted_at:`, `retracted_by:`), as does assertion context on the SQLite
  side. Any id or prov change must carry these.
- **`bin/lemmalog-lint` parses the TSV** (`LEMMALOG1` header, `FACT edge`
  rows) and a pre-commit hook enforces it. A format change breaks the lint and
  the hook together.
- **Round-trip is not byte-exact today**: an empty `extra_rules` reads as
  `RULES` and writes back as `RULES\t`. One byte, cosmetic, but it dirties a
  line on every conversion — noise in exactly the diffs we are trying to clean.
- **`DEFAULT_RULES` is the bootstrap batch and is deliberately not persisted**,
  so rules can change under an existing store. Fine today; a trap if merge
  logic ever depends on rule identity.

## Discarded, with reasons

- **Put the snapshot on `integration` so every branch inherits it.** Does not
  help: it makes collisions *more* likely, because every branch then starts
  from the same counter and both allocate the same next id.
- **Hand-merge the conflicted snapshot.** Forbidden by the team protocol, and
  now we know why: it produces the silent-provenance corruption above. The
  documented fallback — keep one side, re-observe the other's episodes — is
  correct but lossy (replayed facts get a new `valid_from`/`asserted_at`).
- **Switch the shared store to SQLite `.db`.** Binary, so git cannot merge or
  diff it at all — strictly worse for sharing. Also breaks `bin/lemmalog-lint`,
  `make lemmalog-lint` and the pre-commit hook, which all parse TSV.
- **Put a live SQLite store on a shared filesystem / Dropbox / NFS.** SQLite
  locking is unreliable over network filesystems; expect corruption or silent
  conflict copies.
- **Keep the TSV purely as an export format for sharing.** This is the status
  quo, not a fix — it still rewrites whole and merges badly. It suits one
  writer plus readers, which is what the protocol already mandates.

## Leads, NOT evaluated

Recorded so the next session does not re-derive them. Neither has been studied,
costed, or checked against the gotchas above.

1. **Globally unique episode ids** (content hash of the episode, or ULID) so
   two branches cannot collide by construction. Open: migrating existing
   `ep1..ep25` ids and every provenance string that cites them; `convert` is
   the natural place since it already walks every record, but it is a breaking
   format change, not a patch.
2. **Deterministic serialisation** (sort relations, facts and episodes before
   writing). Small, independent, and would stop the spurious full-file diffs on
   its own. Could land first.

If both hold, two branches adding different episodes become non-overlapping
line insertions that git merges with no conflict, and a real conflict means two
people genuinely disagree about the world. That is the outcome to aim at — it
is not yet a plan.

## Framing worth keeping

The store is branch-scoped content, but the facts are not about a branch —
they are about the codebase. That mismatch is the git-DAG valid-time question
[ADR 3](../../docs/adr/0003-defer-dag-valid-time.md) deferred, resurfacing as a
file-layout and merge problem instead of a modelling one. Whatever is decided
here should be checked against that ADR, which may need revisiting rather than
working around.

## Until then

One writer at a time, on one branch. That is what the team protocol already
says; this file is the reason.
