# 3. Defer git-DAG valid-time semantics; buy the columns now

**Status:** accepted · 2026-09-20

Bitemporal models assume valid time is a line; git is a DAG, so "when was this
true of the code" has no single answer across branches. We decline to decide
this yet. Valid time runs on wall clock, and every fact additionally records
`asserted_at_sha` and `asserted_on_branch` — both free from git at write time,
and together enough to retro-fit either commit-anchored valid time or
provisional-branch semantics later.

**Consequence:** we deliberately buy the option rather than the decision,
because the information needed to decide well does not exist yet — we have never
observed a fact that actually differs between two branches. Revisit when one
appears. Related: `valid_from` uses `i64::MIN` as an open-at-the-left sentinel
(mirroring the existing `i64::MAX` for `valid_to`) so "predates recorded
history" stays distinct from "introduced at commit X" — much of this monolith is
older than its own git history.
