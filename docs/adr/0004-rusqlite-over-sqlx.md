# 4. rusqlite, not sqlx

**Status:** accepted · 2026-09-20

The steering decision was "`sqlx` with the SQLite driver, so swapping to Postgres
later is a driver change". Implementation showed the premise was wrong: `sqlx` is
async-only, so it pulls **tokio and its ecosystem** into a crate whose entire
dependency set is two optional crates (`serde_json`, `ureq`), and every
persistence signature here — `save(&self, path) -> io::Result<()>` and its
siblings — is synchronous. Adopting it would mean a `block_on` at each seam or an
async rewrite of the API. Measured alternative: `rusqlite` with bundled SQLite
takes the tree from 6 to 26 dependency lines and builds in ~32s cold.

**Consequence:** we keep portability where it is cheap — `STRICT` tables and a
portable SQL subset — rather than buying it through a shared driver API. A later
Postgres move is a driver swap plus a handful of type mappings, using the
synchronous `postgres` crate, so the sync API survives it. We lose "one API, two
backends"; we keep the actual swap, and we keep [[0001]]'s promise that this
fork stays small enough to rebase on upstream. Recorded because the reasoning
otherwise survives only in a document marked superseded, and because it reverses
an explicit earlier decision — which is exactly the kind of thing that looks like
an accident a year later.
