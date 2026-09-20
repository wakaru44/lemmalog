//! Declared relation vocabulary — parser and validator for `ontology.yaml`.
//!
//! This module reads the ontology file and answers two questions about a
//! candidate fact `Subject --relation--> Object`:
//!
//! - what is the relation's **cardinality** (`single` / `accumulating`), which
//!   is what the update policy acts on; and
//! - does the fact **violate** the declared guard rails (domain, range,
//!   evidence obligation).
//!
//! Nothing here is wired into the engine yet: `apply_update` still classifies
//! by string prefix. See `docs/ontology.md` for the migration.
//!
//! # Policy: strict on relations, lenient on entity kinds
//!
//! [`Violation::is_blocking`] is the whole asymmetry, made explicit so a
//! caller cannot conflate the two by accident:
//!
//! - [`Violation::UnknownRelation`] blocks. An unknown relation has no
//!   cardinality, so the store would have to guess — the bug the file exists
//!   to remove.
//! - [`Violation::DomainMismatch`] / [`Violation::RangeMismatch`] block. The
//!   kind is declared and the declaration says no.
//! - [`Violation::UnknownEntityKind`] does **not** block. The vocabulary of a
//!   system under investigation is open; the flag is the backlog.
//! - [`Violation::MissingEvidence`] does **not** block. It is an obligation,
//!   not a verdict: [`Ontology::check`] sees one fact at a time and cannot
//!   know whether the sibling `evidence` fact lands later in the same batch.
//!   It is emitted whenever the relation declares `requires_evidence: true`;
//!   the caller clears it with a post-batch sweep.
//!
//! # Prefix-less entities are `literal`
//!
//! An entity's kind is the `prefix:` segment before its first `:`, where the
//! prefix is a plain identifier (`[A-Za-z0-9_-]+`). A symbol with no such
//! prefix — `"orders"`, `"does the dunning run"`, `42` — is treated as kind
//! **`literal`**, matching the pseudo-kind the file itself declares for "a
//! bare value, string or number sitting on the object side". It therefore
//! satisfies a domain/range listing `literal` or `any`, and violates one that
//! lists neither. (The alternative — "no kind, matches only `any`" — would
//! reject `purpose`, whose range is `[literal]` and whose objects are always
//! bare sentences.)
//!
//! # Supported YAML subset
//!
//! Hand-rolled, deliberately tiny, and strict: anything outside the subset is
//! an `Err` naming the line, never a guess. Supported: block mappings nested
//! by indentation, plain scalars, single/double-quoted scalars, flow sequences
//! (`[a, b]`), flow mappings (`{k: v}`), folded/literal block scalars (`>`,
//! `>-`, `|`, `|-`), whole-line `#` comments, blank lines. Rejected: block
//! sequences (`- item`), anchors/aliases/tags, multi-document streams, tabs,
//! trailing comments on a value line, and commas or colons inside flow
//! collections.

use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// Minimal YAML node
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Scalar(String),
    Seq(Vec<String>),
    Map(Vec<(String, Node)>),
}

impl Node {
    fn get(&self, key: &str) -> Option<&Node> {
        match self {
            Node::Map(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn as_scalar(&self) -> Option<&str> {
        match self {
            Node::Scalar(s) => Some(s.as_str()),
            _ => None,
        }
    }

    fn as_map(&self) -> Option<&[(String, Node)]> {
        match self {
            Node::Map(entries) => Some(entries),
            _ => None,
        }
    }

    fn as_seq(&self) -> Option<&[String]> {
        match self {
            Node::Seq(items) => Some(items.as_slice()),
            _ => None,
        }
    }
}

struct Line<'a> {
    no: usize,
    indent: usize,
    text: &'a str,
}

/// Drop blanks and whole-line comments; reject tabs and stream markers.
fn scan(yaml: &str) -> Result<Vec<Line<'_>>, String> {
    let mut out = Vec::new();
    for (idx, raw) in yaml.lines().enumerate() {
        let no = idx + 1;
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.contains('\t') {
            return Err(format!("line {no}: tabs are not supported in indentation"));
        }
        let indent = line.len() - line.trim_start_matches(' ').len();
        let text = line[indent..].trim_end();
        if text.is_empty() || text.starts_with('#') {
            continue;
        }
        if text == "---" || text == "..." {
            return Err(format!("line {no}: multi-document streams are not supported"));
        }
        out.push(Line { no, indent, text });
    }
    Ok(out)
}

/// Parse a block mapping whose entries all sit at `indent`.
fn parse_map(lines: &[Line<'_>], i: &mut usize, indent: usize) -> Result<Node, String> {
    let mut entries: Vec<(String, Node)> = Vec::new();
    while *i < lines.len() {
        let line = &lines[*i];
        if line.indent < indent {
            break;
        }
        if line.indent > indent {
            return Err(format!("line {}: unexpected indentation", line.no));
        }
        if line.text.starts_with("- ") || line.text == "-" {
            return Err(format!(
                "line {}: block sequences are not supported (use flow style `[a, b]`)",
                line.no
            ));
        }
        if line.text.starts_with('&') || line.text.starts_with('*') {
            return Err(format!("line {}: anchors and aliases are not supported", line.no));
        }
        let (key, rest) = split_key(line.text, line.no)?;
        if entries.iter().any(|(k, _)| k == &key) {
            return Err(format!("line {}: duplicate key `{key}`", line.no));
        }
        let no = line.no;
        *i += 1;
        let value = if rest.is_empty() {
            let child_indent = match lines.get(*i) {
                Some(next) if next.indent > indent => next.indent,
                _ => return Err(format!("line {no}: `{key}:` has no value and no nested block")),
            };
            parse_map(lines, i, child_indent)?
        } else if matches!(rest, ">" | ">-" | ">+" | "|" | "|-" | "|+") {
            Node::Scalar(parse_block_scalar(lines, i, indent, rest.starts_with('|')))
        } else {
            parse_flow(rest, no)?
        };
        entries.push((key, value));
    }
    Ok(Node::Map(entries))
}

/// Split `key: rest`, requiring the colon to be followed by a space or EOL so
/// colons inside plain scalars do not split the line.
fn split_key(text: &str, no: usize) -> Result<(String, &str), String> {
    let bytes = text.as_bytes();
    for (p, b) in bytes.iter().enumerate() {
        if *b == b':' && (p + 1 == bytes.len() || bytes[p + 1] == b' ') {
            let key = text[..p].trim();
            if key.is_empty() {
                return Err(format!("line {no}: empty key"));
            }
            if key.starts_with('"') || key.starts_with('\'') || key.starts_with('[') {
                return Err(format!("line {no}: only plain scalar keys are supported"));
            }
            return Ok((key.to_string(), text[p + 1..].trim()));
        }
    }
    Err(format!("line {no}: expected `key: value`, got `{text}`"))
}

/// Collect the more-indented continuation lines of a block scalar. Folded
/// (`>`) joins with spaces; literal (`|`) joins with newlines.
fn parse_block_scalar(lines: &[Line<'_>], i: &mut usize, indent: usize, literal: bool) -> String {
    let mut parts: Vec<&str> = Vec::new();
    while let Some(line) = lines.get(*i) {
        if line.indent <= indent {
            break;
        }
        parts.push(line.text);
        *i += 1;
    }
    parts.join(if literal { "\n" } else { " " })
}

/// Parse the right-hand side of a `key:` — flow collection, quoted or plain.
fn parse_flow(rest: &str, no: usize) -> Result<Node, String> {
    if let Some(inner) = rest.strip_prefix('[') {
        let inner = inner
            .strip_suffix(']')
            .ok_or_else(|| format!("line {no}: unterminated flow sequence"))?;
        if inner.trim().is_empty() {
            return Ok(Node::Seq(Vec::new()));
        }
        let mut items = Vec::new();
        for part in inner.split(',') {
            items.push(unquote(part.trim(), no)?);
        }
        return Ok(Node::Seq(items));
    }
    if let Some(inner) = rest.strip_prefix('{') {
        let inner = inner
            .strip_suffix('}')
            .ok_or_else(|| format!("line {no}: unterminated flow mapping"))?;
        let mut entries = Vec::new();
        for part in inner.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let (k, v) = split_key(part, no)?;
            entries.push((k, Node::Scalar(unquote(v, no)?)));
        }
        return Ok(Node::Map(entries));
    }
    if rest.contains(": ") || rest.ends_with(':') {
        return Err(format!("line {no}: unexpected `:` in plain scalar `{rest}`"));
    }
    Ok(Node::Scalar(unquote(rest, no)?))
}

fn unquote(s: &str, no: usize) -> Result<String, String> {
    for q in ['"', '\''] {
        if let Some(body) = s.strip_prefix(q) {
            let body = body
                .strip_suffix(q)
                .ok_or_else(|| format!("line {no}: unterminated quoted scalar"))?;
            if body.contains(q) {
                return Err(format!("line {no}: escapes inside quoted scalars are not supported"));
            }
            return Ok(body.replace("\\n", "\n"));
        }
    }
    if s.contains('[') || s.contains(']') || s.contains('{') || s.contains('}') {
        return Err(format!("line {no}: unexpected flow indicator in scalar `{s}`"));
    }
    Ok(s.to_string())
}

// ---------------------------------------------------------------------------
// Ontology
// ---------------------------------------------------------------------------

/// How many values of a relation may be open at once for one subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cardinality {
    /// One open value; a new value supersedes the last.
    Single,
    /// Many values coexist; new ones add.
    Accumulating,
}

/// One declared relation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationDef {
    pub cardinality: Cardinality,
    pub domain: Vec<String>,
    pub range: Vec<String>,
    pub requires_evidence: bool,
    pub description: String,
}

/// A candidate fact's disagreement with the declared vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    UnknownRelation {
        relation: String,
    },
    UnknownEntityKind {
        entity: String,
        kind: String,
    },
    DomainMismatch {
        relation: String,
        entity: String,
        expected: String,
    },
    RangeMismatch {
        relation: String,
        entity: String,
        expected: String,
    },
    MissingEvidence {
        relation: String,
    },
}

impl Violation {
    /// `true` when the write must be rejected, `false` when it is a flag the
    /// caller records and carries on. See the module docs for the rationale.
    pub fn is_blocking(&self) -> bool {
        matches!(
            self,
            Violation::UnknownRelation { .. }
                | Violation::DomainMismatch { .. }
                | Violation::RangeMismatch { .. }
        )
    }
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Violation::UnknownRelation { relation } => {
                write!(f, "unknown relation `{relation}`")
            }
            Violation::UnknownEntityKind { entity, kind } => {
                write!(f, "unknown entity kind `{kind}:` on `{entity}`")
            }
            Violation::DomainMismatch {
                relation,
                entity,
                expected,
            } => write!(
                f,
                "`{entity}` is not a valid subject of `{relation}` (expected {expected})"
            ),
            Violation::RangeMismatch {
                relation,
                entity,
                expected,
            } => write!(
                f,
                "`{entity}` is not a valid object of `{relation}` (expected {expected})"
            ),
            Violation::MissingEvidence { relation } => {
                write!(f, "`{relation}` requires a sibling `evidence` fact")
            }
        }
    }
}

/// The parsed `ontology.yaml`.
#[derive(Debug, Clone)]
pub struct Ontology {
    version: String,
    relations: BTreeMap<String, RelationDef>,
    entity_kinds: BTreeSet<String>,
    policy: BTreeMap<String, String>,
}

impl Ontology {
    pub fn from_path(path: &str) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        Self::from_str(&text)
    }

    #[allow(clippy::should_implement_trait)] // fallible, but not `FromStr`: the error is a String
    pub fn from_str(yaml: &str) -> Result<Self, String> {
        let lines = scan(yaml)?;
        let mut i = 0usize;
        let root = parse_map(&lines, &mut i, 0)?;

        let version = root
            .get("version")
            .and_then(Node::as_scalar)
            .unwrap_or("0")
            .to_string();

        let mut policy = BTreeMap::new();
        if let Some(node) = root.get("policy") {
            let entries = node
                .as_map()
                .ok_or_else(|| "`policy` must be a mapping".to_string())?;
            for (k, v) in entries {
                let value = v
                    .as_scalar()
                    .ok_or_else(|| format!("policy.{k} must be a scalar"))?;
                policy.insert(k.clone(), value.to_string());
            }
        }

        let kinds_node = root
            .get("entity_kinds")
            .ok_or_else(|| "missing top-level `entity_kinds`".to_string())?;
        let mut entity_kinds = BTreeSet::new();
        for (name, _) in kinds_node
            .as_map()
            .ok_or_else(|| "`entity_kinds` must be a mapping".to_string())?
        {
            entity_kinds.insert(name.clone());
        }

        let relations_node = root
            .get("relations")
            .ok_or_else(|| "missing top-level `relations`".to_string())?;
        let mut relations = BTreeMap::new();
        for (name, body) in relations_node
            .as_map()
            .ok_or_else(|| "`relations` must be a mapping".to_string())?
        {
            relations.insert(name.clone(), relation_def(name, body)?);
        }

        Ok(Ontology {
            version,
            relations,
            entity_kinds,
            policy,
        })
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    /// A declared policy value, e.g. `policy("unknown_relations")`.
    pub fn policy(&self, key: &str) -> Option<&str> {
        self.policy.get(key).map(String::as_str)
    }

    pub fn relation(&self, relation: &str) -> Option<&RelationDef> {
        self.relations.get(relation)
    }

    pub fn relation_names(&self) -> impl Iterator<Item = &str> {
        self.relations.keys().map(String::as_str)
    }

    pub fn entity_kind_names(&self) -> impl Iterator<Item = &str> {
        self.entity_kinds.iter().map(String::as_str)
    }

    pub fn cardinality(&self, relation: &str) -> Option<Cardinality> {
        self.relations.get(relation).map(|d| d.cardinality)
    }

    /// The kind of an entity symbol: its `prefix:` segment when that prefix is
    /// a plain identifier, otherwise `literal`. See the module docs.
    pub fn kind_of(entity: &str) -> &str {
        match entity.split_once(':') {
            Some((prefix, _))
                if !prefix.is_empty()
                    && prefix
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') =>
            {
                prefix
            }
            _ => "literal",
        }
    }

    /// Validate one candidate fact. An empty result means it conforms.
    ///
    /// Strict on relations, lenient on entity kinds: use
    /// [`Violation::is_blocking`] rather than emptiness to decide whether to
    /// reject the write.
    pub fn check(&self, subject: &str, relation: &str, object: &str) -> Vec<Violation> {
        let mut out = Vec::new();
        let def = match self.relations.get(relation) {
            Some(def) => def,
            None => {
                out.push(Violation::UnknownRelation {
                    relation: relation.to_string(),
                });
                return out;
            }
        };
        self.check_side(subject, &def.domain, relation, true, &mut out);
        self.check_side(object, &def.range, relation, false, &mut out);
        if def.requires_evidence {
            out.push(Violation::MissingEvidence {
                relation: relation.to_string(),
            });
        }
        out
    }

    fn check_side(
        &self,
        entity: &str,
        allowed: &[String],
        relation: &str,
        is_domain: bool,
        out: &mut Vec<Violation>,
    ) {
        let kind = Self::kind_of(entity);
        if !self.entity_kinds.contains(kind) {
            // Open vocabulary: flag it, and do not judge it against a
            // declaration it was never measured by.
            out.push(Violation::UnknownEntityKind {
                entity: entity.to_string(),
                kind: kind.to_string(),
            });
            return;
        }
        if allowed.iter().any(|a| a == "any" || a == kind) {
            return;
        }
        let expected = allowed.join(", ");
        out.push(if is_domain {
            Violation::DomainMismatch {
                relation: relation.to_string(),
                entity: entity.to_string(),
                expected,
            }
        } else {
            Violation::RangeMismatch {
                relation: relation.to_string(),
                entity: entity.to_string(),
                expected,
            }
        });
    }
}

fn relation_def(name: &str, body: &Node) -> Result<RelationDef, String> {
    let field = |key: &str| -> Result<&Node, String> {
        body.get(key)
            .ok_or_else(|| format!("relation `{name}`: missing `{key}`"))
    };
    let cardinality = match field("cardinality")?.as_scalar() {
        Some("single") => Cardinality::Single,
        Some("accumulating") => Cardinality::Accumulating,
        other => {
            return Err(format!(
                "relation `{name}`: cardinality must be `single` or `accumulating`, got {other:?}"
            ))
        }
    };
    let kinds = |key: &str| -> Result<Vec<String>, String> {
        field(key)?
            .as_seq()
            .map(<[String]>::to_vec)
            .ok_or_else(|| format!("relation `{name}`: `{key}` must be a flow sequence"))
    };
    let requires_evidence = match field("requires_evidence")?.as_scalar() {
        Some("true") => true,
        Some("false") => false,
        other => {
            return Err(format!(
                "relation `{name}`: requires_evidence must be true or false, got {other:?}"
            ))
        }
    };
    Ok(RelationDef {
        cardinality,
        domain: kinds("domain")?,
        range: kinds("range")?,
        requires_evidence,
        description: body
            .get("description")
            .and_then(Node::as_scalar)
            .unwrap_or_default()
            .to_string(),
    })
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const REAL: &str = "ontology.yaml";

    fn load() -> Ontology {
        match Ontology::from_path(REAL) {
            Ok(o) => o,
            Err(e) => panic!("ontology.yaml failed to parse: {e}"),
        }
    }

    /// Independent count: top-level keys of a section, straight off the text,
    /// without going through the parser under test.
    fn declared_keys(section: &str) -> usize {
        let text = std::fs::read_to_string(REAL).expect("read ontology.yaml");
        let mut inside = false;
        let mut n = 0;
        for line in text.lines() {
            if !line.starts_with(' ') && !line.trim().is_empty() && !line.starts_with('#') {
                inside = line.trim_end() == format!("{section}:");
                continue;
            }
            let trimmed = line.trim_start();
            if inside
                && line.len() - trimmed.len() == 2
                && trimmed.ends_with(':')
                && !trimmed.starts_with('#')
            {
                n += 1;
            }
        }
        n
    }

    #[test]
    fn parses_the_committed_ontology() {
        let onto = load();
        assert_eq!(onto.relation_names().count(), declared_keys("relations"));
        assert_eq!(
            onto.entity_kind_names().count(),
            declared_keys("entity_kinds")
        );
        assert_eq!(onto.version(), "1");
        assert_eq!(onto.policy("unknown_relations"), Some("reject"));
        assert_eq!(onto.policy("unknown_entity_kinds"), Some("accept_and_flag"));
    }

    #[test]
    fn valid_fact_passes() {
        let onto = load();
        // `calls`: domain [method, module, class], range [method, module],
        // requires_evidence false.
        assert_eq!(onto.check("method:Invoice::send", "calls", "method:Mailer::deliver"), vec![]);
        // Prefix-less object against a `[literal]` range.
        assert_eq!(onto.check("table:orders", "purpose", "holds one row per order"), vec![]);
    }

    #[test]
    fn unknown_relation_blocks() {
        let onto = load();
        let v = onto.check("method:a", "frobnicates", "method:b");
        assert_eq!(
            v,
            vec![Violation::UnknownRelation {
                relation: "frobnicates".into()
            }]
        );
        assert!(v[0].is_blocking());
    }

    #[test]
    fn unknown_entity_kind_flags_but_does_not_block() {
        let onto = load();
        let v = onto.check("job:nightly_dunning", "calls", "method:Mailer::deliver");
        assert_eq!(
            v,
            vec![Violation::UnknownEntityKind {
                entity: "job:nightly_dunning".into(),
                kind: "job".into()
            }]
        );
        assert!(!v[0].is_blocking());
    }

    #[test]
    fn domain_and_range_mismatches_block() {
        let onto = load();
        let v = onto.check("table:orders", "calls", "method:Mailer::deliver");
        assert!(matches!(v.as_slice(), [Violation::DomainMismatch { .. }]));
        assert!(v[0].is_blocking());

        let v = onto.check("method:Invoice::send", "calls", "table:orders");
        assert!(matches!(v.as_slice(), [Violation::RangeMismatch { .. }]));
        assert!(v[0].is_blocking());
    }

    #[test]
    fn requires_evidence_warns_only() {
        let onto = load();
        // `stored_in`: domain [entity, enum, term], range [table],
        // requires_evidence true.
        let v = onto.check("entity:invoice", "stored_in", "table:invoices");
        assert_eq!(
            v,
            vec![Violation::MissingEvidence {
                relation: "stored_in".into()
            }]
        );
        assert!(!v[0].is_blocking());
    }

    #[test]
    fn cardinality_matches_the_declaration() {
        let onto = load();
        assert_eq!(onto.cardinality("status"), Some(Cardinality::Single));
        assert_eq!(onto.cardinality("works_at"), Some(Cardinality::Single));
        assert_eq!(onto.cardinality("calls"), Some(Cardinality::Accumulating));
        assert_eq!(onto.cardinality("evidence"), Some(Cardinality::Accumulating));
        assert_eq!(onto.cardinality("frobnicates"), None);
    }

    #[test]
    fn folded_block_scalar_is_folded() {
        let onto = load();
        // `works_at.legacy.note` is a `>-` block; it must have parsed without
        // swallowing the following relations.
        assert!(onto.relation("works_at").is_some());
        assert_eq!(
            onto.relation("purpose").map(|d| d.cardinality),
            Some(Cardinality::Single)
        );
    }

    #[test]
    fn prefixless_entities_are_literal() {
        assert_eq!(Ontology::kind_of("table:orders"), "table");
        assert_eq!(Ontology::kind_of("orders"), "literal");
        assert_eq!(Ontology::kind_of("a sentence: with a colon"), "literal");
        assert_eq!(Ontology::kind_of(""), "literal");
    }

    #[test]
    fn malformed_input_errors_rather_than_panics() {
        for bad in [
            "relations:\n  calls:\n      cardinality: accumulating\n",
            "relations:\n  - calls\n",
            "entity_kinds:\n\trelations: x\n",
            "not a mapping",
            "version: 1\nrelations:\n",
            "relations:\n  calls:\n    cardinality: bogus\n    domain: [any]\n    range: [any]\n    requires_evidence: false\nentity_kinds:\n  any:\n    description: x\n",
            "relations:\n  calls:\n    domain: [any\n",
        ] {
            assert!(
                Ontology::from_str(bad).is_err(),
                "expected Err for input: {bad:?}"
            );
        }
    }

    #[test]
    fn minimal_hand_written_ontology_round_trips() {
        let src = "\
version: 2

policy:
  unknown_relations: reject

entity_kinds:
  table:
    description: A table.
  literal:
    description: A bare value.
  any:
    description: No constraint.

relations:
  purpose:
    cardinality: single
    domain: [any]
    range: [literal]
    requires_evidence: false
    description: What it is for.
  reads_table:
    cardinality: accumulating
    domain: [any]
    range: [table]
    requires_evidence: true
    description: >-
      Reads rows
      from the table.
";
        let onto = match Ontology::from_str(src) {
            Ok(o) => o,
            Err(e) => panic!("{e}"),
        };
        assert_eq!(onto.version(), "2");
        assert_eq!(onto.relation_names().count(), 2);
        assert_eq!(
            onto.relation("reads_table").map(|d| d.description.as_str()),
            Some("Reads rows from the table.")
        );
        assert_eq!(onto.check("table:x", "purpose", "to hold rows"), vec![]);
    }
}
