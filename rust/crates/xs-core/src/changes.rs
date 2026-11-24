//! Platform-independent, transactional change-journal model.
//!
//! This module does **not** read NTFS, install a service, or claim USN freshness.
//! A future platform adapter must translate observed records into these events
//! and persist the resulting graph and cursor in the same checkpoint. Stable
//! IDs, not vector slots or materialized paths, define parent relationships.
//!
//! Adapter limitations: one canonical name/parent is modeled per file ID;
//! multiple hard-link directory entries require a separate edge model before
//! production NTFS ingestion. Reparse points and volume identity must be
//! authorized by the platform layer. `apply_batch` is atomic in memory; the
//! caller must atomically persist graph + journal ID + cursor together. No
//! operating-system freshness or performance target is established here.

use serde::{
    de::Error as DeserializeError,
    ser::{SerializeSeq, SerializeStruct},
    Deserialize, Deserializer, Serialize, Serializer,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
};

const SCHEMA_VERSION: u32 = 1;
pub const MAX_BATCH_EVENTS: usize = 100_000;
pub const MAX_GRAPH_DEPTH: usize = 1024;
pub const MAX_NODE_NAME_BYTES: usize = 4096;
pub const MAX_NODE_NAME_CHARS: usize = 1024;
const MAX_GRAPH_NODES: usize = 10_000_000;

/// Supports both 64-bit and 128-bit filesystem identities. JSON always uses a
/// fixed-width hexadecimal string so JavaScript/JSON numeric rounding cannot
/// merge two different file identities. NTFS adapters must retain the sequence
/// component of a file reference; an MFT slot number alone is not a stable ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(pub u128);
impl From<u64> for FileId {
    fn from(value: u64) -> Self {
        Self(u128::from(value))
    }
}
impl fmt::Display for FileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}
impl Serialize for FileId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}
impl<'de> Deserialize<'de> for FileId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value.len() != 32 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(D::Error::custom(
                "file ID must be a 32-digit hexadecimal string",
            ));
        }
        u128::from_str_radix(&value, 16)
            .map(Self)
            .map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PathFlavor {
    Windows,
    Posix,
}
impl PathFlavor {
    fn separator(self) -> char {
        match self {
            Self::Windows => '\\',
            Self::Posix => '/',
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileNode {
    pub id: FileId,
    pub parent_id: FileId,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// Unix UTC seconds, same convention as xs-query and FileRecord.
    pub modified: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Change {
    /// Full metadata makes a rename/create/modify independent of a preceding
    /// old-name record and replaces the same identity instead of appending it.
    Upsert { node: FileNode },
    /// A split old-name notification does not hide or duplicate the entry.
    /// Its new-name event can safely arrive in a later batch.
    RenameOld { id: FileId },
    /// For a previously known ID. If metadata is unavailable, the adapter must
    /// supply a full Upsert rather than inventing size/type information.
    Rename {
        id: FileId,
        parent_id: FileId,
        name: String,
    },
    /// Removes all known descendants even if child-delete events never arrive.
    Delete { id: FileId },
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChangeEvent {
    pub usn: i64,
    pub change: Change,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct JournalBatch {
    pub journal_id: u64,
    /// Inclusive read cursor used by the platform provider.
    pub start_cursor: i64,
    /// Exclusive next cursor returned by the provider.
    pub end_cursor: i64,
    /// Strictly increasing USNs, each in [start_cursor, end_cursor).
    pub events: Vec<ChangeEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum RebuildReason {
    JournalChanged { expected: u64, observed: u64 },
    CursorGap { expected: i64, observed: i64 },
    CursorOverlap { expected: i64, observed: i64 },
    RootChanged,
    MissingRenameMetadata { id: FileId },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeError {
    InvalidState(String),
    InvalidBatch(String),
    RebuildRequired(RebuildReason),
}
impl fmt::Display for ChangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidState(message) => write!(f, "invalid journal state: {message}"),
            Self::InvalidBatch(message) => write!(f, "invalid journal batch: {message}"),
            Self::RebuildRequired(reason) => {
                write!(f, "journal baseline rebuild required: {reason:?}")
            }
        }
    }
}
impl Error for ChangeError {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ApplyOutcome {
    Applied {
        cursor: i64,
        /// New or modified nodes in the final state, counting an ID once.
        upserted: usize,
        removed: usize,
        renamed: usize,
        old_name_events: usize,
    },
    AlreadyApplied {
        cursor: i64,
    },
}

/// Invariants are maintained by constructors, deserialization and apply_batch;
/// callers receive only read access to nodes and the journal cursor. Names and
/// parent IDs are stored once; paths are derived through the current graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalState {
    journal_id: u64,
    cursor: i64,
    root_id: FileId,
    root_path: String,
    flavor: PathFlavor,
    nodes: BTreeMap<FileId, FileNode>,
    /// Derived adjacency is rebuilt on load and never trusted from JSON.
    children: BTreeMap<FileId, BTreeSet<FileId>>,
}

impl JournalState {
    pub fn new(
        journal_id: u64,
        cursor: i64,
        root_id: FileId,
        root_path: String,
        flavor: PathFlavor,
    ) -> Result<Self, ChangeError> {
        if cursor < 0 {
            return Err(ChangeError::InvalidState("negative journal cursor".into()));
        }
        let root_path = normalize_root(&root_path, flavor)?;
        Ok(Self {
            journal_id,
            cursor,
            root_id,
            root_path,
            flavor,
            nodes: BTreeMap::new(),
            children: BTreeMap::new(),
        })
    }
    pub fn journal_id(&self) -> u64 {
        self.journal_id
    }
    pub fn cursor(&self) -> i64 {
        self.cursor
    }
    pub fn root_id(&self) -> FileId {
        self.root_id
    }
    pub fn root_path(&self) -> &str {
        &self.root_path
    }
    pub fn flavor(&self) -> PathFlavor {
        self.flavor
    }
    pub fn len(&self) -> usize {
        self.nodes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
    pub fn get(&self, id: FileId) -> Option<&FileNode> {
        self.nodes.get(&id)
    }
    pub fn iter(&self) -> impl Iterator<Item = &FileNode> {
        self.nodes.values()
    }

    /// Unknown IDs and nodes whose parent chain is unresolved produce None.
    /// In particular, a missing parent is never treated as the volume root.
    pub fn resolve_path(&self, id: FileId) -> Result<Option<String>, ChangeError> {
        let mut current = id;
        let mut parts = Vec::new();
        let mut visited = BTreeSet::new();
        for _ in 0..=MAX_GRAPH_DEPTH {
            if current == self.root_id {
                let mut path = self.root_path.clone();
                let separator = self.flavor.separator();
                for part in parts.into_iter().rev() {
                    if !path.ends_with(separator) {
                        path.push(separator);
                    }
                    path.push_str(part);
                }
                return Ok(Some(path));
            }
            if !visited.insert(current) {
                return Err(ChangeError::InvalidState("cycle in parent graph".into()));
            }
            let Some(node) = self.nodes.get(&current) else {
                return Ok(None);
            };
            parts.push(node.name.as_str());
            current = node.parent_id;
        }
        Err(ChangeError::InvalidState(
            "parent graph exceeds depth budget".into(),
        ))
    }

    /// Build changes in a bounded transaction overlay, validate its final graph,
    /// then commit nodes, adjacency and cursor together. An invalid batch has no
    /// visible effect, including errors encountered after earlier valid events.
    /// A normal batch does not clone the full volume graph.
    pub fn apply_batch(&mut self, batch: &JournalBatch) -> Result<ApplyOutcome, ChangeError> {
        self.validate_batch(batch)?;
        if batch.end_cursor <= self.cursor {
            return Ok(ApplyOutcome::AlreadyApplied {
                cursor: self.cursor,
            });
        }
        if batch.start_cursor > self.cursor {
            return Err(ChangeError::RebuildRequired(RebuildReason::CursorGap {
                expected: self.cursor,
                observed: batch.start_cursor,
            }));
        }
        if batch.start_cursor < self.cursor {
            return Err(ChangeError::RebuildRequired(RebuildReason::CursorOverlap {
                expected: self.cursor,
                observed: batch.start_cursor,
            }));
        }
        let mut transaction = Transaction::new(self);
        let mut old_name_events = 0;
        for event in &batch.events {
            match &event.change {
                Change::Upsert { node } => transaction.upsert(node.clone()),
                Change::RenameOld { .. } => old_name_events += 1,
                Change::Rename {
                    id,
                    parent_id,
                    name,
                } => {
                    let mut node = transaction.get(*id).cloned().ok_or({
                        ChangeError::RebuildRequired(RebuildReason::MissingRenameMetadata {
                            id: *id,
                        })
                    })?;
                    node.parent_id = *parent_id;
                    node.name.clone_from(name);
                    transaction.upsert(node);
                }
                Change::Delete { id } => transaction.delete_subtree(*id)?,
            }
        }
        transaction.validate()?;
        let delta = transaction.delta;
        let mut upserted = 0;
        let mut removed = 0;
        let mut renamed = 0;
        // No fallible validation occurs after this point.
        for (id, replacement) in delta {
            if let Some(previous) = self.nodes.get(&id) {
                if replacement.as_ref() == Some(previous) {
                    continue;
                }
                if replacement.as_ref().is_some_and(|next| {
                    next.name != previous.name || next.parent_id != previous.parent_id
                }) {
                    renamed += 1;
                }
                if let Some(children) = self.children.get_mut(&previous.parent_id) {
                    children.remove(&id);
                    if children.is_empty() {
                        self.children.remove(&previous.parent_id);
                    }
                }
            } else if replacement.is_none() {
                continue;
            }
            match replacement {
                Some(node) => {
                    self.children.entry(node.parent_id).or_default().insert(id);
                    self.nodes.insert(id, node);
                    upserted += 1;
                }
                None => {
                    self.nodes.remove(&id);
                    removed += 1;
                }
            }
        }
        self.cursor = batch.end_cursor;
        Ok(ApplyOutcome::Applied {
            cursor: self.cursor,
            upserted,
            removed,
            renamed,
            old_name_events,
        })
    }

    fn validate_batch(&self, batch: &JournalBatch) -> Result<(), ChangeError> {
        if batch.journal_id != self.journal_id {
            return Err(ChangeError::RebuildRequired(
                RebuildReason::JournalChanged {
                    expected: self.journal_id,
                    observed: batch.journal_id,
                },
            ));
        }
        if batch.start_cursor < 0 || batch.end_cursor < batch.start_cursor {
            return Err(ChangeError::InvalidBatch(
                "negative or reversed cursor range".into(),
            ));
        }
        if batch.events.len() > MAX_BATCH_EVENTS {
            return Err(ChangeError::InvalidBatch(format!(
                "batch exceeds {MAX_BATCH_EVENTS} event budget"
            )));
        }
        let mut previous = None;
        for event in &batch.events {
            if event.usn < batch.start_cursor
                || event.usn >= batch.end_cursor
                || previous.is_some_and(|usn| event.usn <= usn)
            {
                return Err(ChangeError::InvalidBatch(
                    "USNs must strictly increase within the batch cursor range".into(),
                ));
            }
            previous = Some(event.usn);
            match &event.change {
                Change::Upsert { node } => {
                    self.validate_node(node)
                        .map_err(ChangeError::InvalidBatch)?;
                }
                Change::Rename {
                    id,
                    parent_id,
                    name,
                } => {
                    if id == parent_id {
                        return Err(ChangeError::InvalidBatch(
                            "a file cannot be its own parent".into(),
                        ));
                    }
                    validate_name(name, self.flavor).map_err(ChangeError::InvalidBatch)?;
                    if *id == self.root_id {
                        return Err(ChangeError::RebuildRequired(RebuildReason::RootChanged));
                    }
                }
                Change::Delete { id } | Change::RenameOld { id } => {
                    if *id == self.root_id {
                        return Err(ChangeError::RebuildRequired(RebuildReason::RootChanged));
                    }
                }
            }
        }
        Ok(())
    }
    fn validate_node(&self, node: &FileNode) -> Result<(), String> {
        if node.id == self.root_id {
            return Err("the graph root is reserved and cannot be upserted".into());
        }
        if node.id == node.parent_id {
            return Err("a file cannot be its own parent".into());
        }
        validate_name(&node.name, self.flavor)
    }
    fn rebuild_and_validate(&mut self) -> Result<(), ChangeError> {
        if self.nodes.len() > MAX_GRAPH_NODES {
            return Err(ChangeError::InvalidState(
                "graph exceeds node budget".into(),
            ));
        }
        self.children.clear();
        for node in self.nodes.values() {
            self.validate_node(node)
                .map_err(ChangeError::InvalidState)?;
            if node.parent_id != self.root_id
                && self
                    .nodes
                    .get(&node.parent_id)
                    .is_some_and(|parent| !parent.is_dir)
            {
                return Err(ChangeError::InvalidState(
                    "file used as a parent directory".into(),
                ));
            }
            self.children
                .entry(node.parent_id)
                .or_default()
                .insert(node.id);
        }
        // Cache verified chain depths so loading a volume does not repeatedly
        // walk every ancestor of every sibling.
        let mut known_depth = BTreeMap::from([(self.root_id, 0_usize)]);
        for id in self.nodes.keys() {
            if known_depth.contains_key(id) {
                continue;
            }
            let mut current = *id;
            let mut chain = Vec::new();
            let mut seen = BTreeSet::new();
            let base_depth = loop {
                if let Some(depth) = known_depth.get(&current) {
                    break *depth;
                }
                let Some(node) = self.nodes.get(&current) else {
                    break 0;
                };
                if !seen.insert(current) {
                    return Err(ChangeError::InvalidState("cycle in parent graph".into()));
                }
                chain.push(current);
                if chain.len() > MAX_GRAPH_DEPTH {
                    return Err(ChangeError::InvalidState(
                        "parent graph exceeds depth budget".into(),
                    ));
                }
                current = node.parent_id;
            };
            if base_depth.saturating_add(chain.len()) > MAX_GRAPH_DEPTH {
                return Err(ChangeError::InvalidState(
                    "parent graph exceeds depth budget".into(),
                ));
            }
            for (position, id) in chain.into_iter().rev().enumerate() {
                known_depth.insert(id, base_depth + position + 1);
            }
        }
        Ok(())
    }
}

struct Transaction<'a> {
    base: &'a JournalState,
    delta: BTreeMap<FileId, Option<FileNode>>,
    /// Candidate newly attached children. Stale edges are filtered using the
    /// current overlay, so repeated moves never produce duplicate children.
    additions: BTreeMap<FileId, BTreeSet<FileId>>,
}
impl<'a> Transaction<'a> {
    fn new(base: &'a JournalState) -> Self {
        Self {
            base,
            delta: BTreeMap::new(),
            additions: BTreeMap::new(),
        }
    }
    fn get(&self, id: FileId) -> Option<&FileNode> {
        match self.delta.get(&id) {
            Some(node) => node.as_ref(),
            None => self.base.nodes.get(&id),
        }
    }
    fn upsert(&mut self, node: FileNode) {
        self.additions
            .entry(node.parent_id)
            .or_default()
            .insert(node.id);
        self.delta.insert(node.id, Some(node));
    }
    fn children(&self, id: FileId) -> BTreeSet<FileId> {
        self.base
            .children
            .get(&id)
            .into_iter()
            .flatten()
            .chain(self.additions.get(&id).into_iter().flatten())
            .filter(|child| self.get(**child).is_some_and(|node| node.parent_id == id))
            .copied()
            .collect()
    }
    fn delete_subtree(&mut self, id: FileId) -> Result<(), ChangeError> {
        let mut pending = vec![id];
        let mut seen = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            // Even an unknown directory may have known orphan children. A
            // parent deletion invalidates those as well, without fabricating a
            // root path or waiting for a separate delete event for each child.
            pending.extend(self.children(id));
            if self.get(id).is_some() {
                self.delta.insert(id, None);
            }
            if seen.len() > MAX_GRAPH_NODES {
                return Err(ChangeError::InvalidBatch(
                    "delete subtree exceeds node budget".into(),
                ));
            }
        }
        Ok(())
    }
    fn validate(&self) -> Result<(), ChangeError> {
        let additions = self
            .delta
            .iter()
            .filter(|(id, node)| node.is_some() && !self.base.nodes.contains_key(id))
            .count();
        let removals = self
            .delta
            .iter()
            .filter(|(id, node)| node.is_none() && self.base.nodes.contains_key(id))
            .count();
        if self
            .base
            .len()
            .saturating_add(additions)
            .saturating_sub(removals)
            > MAX_GRAPH_NODES
        {
            return Err(ChangeError::InvalidBatch(
                "graph exceeds node budget".into(),
            ));
        }
        let mut moved_subtrees = Vec::new();
        for node in self.delta.values().flatten() {
            self.base
                .validate_node(node)
                .map_err(ChangeError::InvalidBatch)?;
            if !node.is_dir && !self.children(node.id).is_empty() {
                return Err(ChangeError::InvalidBatch(
                    "a file cannot retain directory children".into(),
                ));
            }
            let mut current = node.id;
            let mut seen = BTreeSet::new();
            for depth in 0..=MAX_GRAPH_DEPTH {
                if current == self.base.root_id {
                    break;
                }
                let Some(candidate) = self.get(current) else {
                    break;
                };
                if depth == MAX_GRAPH_DEPTH {
                    return Err(ChangeError::InvalidBatch(
                        "parent graph exceeds depth budget".into(),
                    ));
                }
                if !seen.insert(current) {
                    return Err(ChangeError::InvalidBatch("cycle in parent graph".into()));
                }
                if candidate.parent_id != self.base.root_id
                    && self
                        .get(candidate.parent_id)
                        .is_some_and(|parent| !parent.is_dir)
                {
                    return Err(ChangeError::InvalidBatch(
                        "file used as a parent directory".into(),
                    ));
                }
                current = candidate.parent_id;
            }
            if node.is_dir
                && self
                    .base
                    .nodes
                    .get(&node.id)
                    .is_none_or(|previous| previous.parent_id != node.parent_id)
            {
                moved_subtrees.push((node.id, seen.len()));
            }
        }
        // Moving a directory can push an unchanged descendant beyond the
        // depth limit. Validate descendants as well as the directly changed
        // ancestors, otherwise an accepted graph could reject its own snapshot
        // on the next load. Shared affected subtrees are visited only once.
        let mut checked = BTreeSet::new();
        while let Some((id, depth)) = moved_subtrees.pop() {
            if !checked.insert(id) {
                continue;
            }
            if depth > MAX_GRAPH_DEPTH {
                return Err(ChangeError::InvalidBatch(
                    "moved subtree exceeds parent depth budget".into(),
                ));
            }
            moved_subtrees.extend(
                self.children(id)
                    .into_iter()
                    .map(|child| (child, depth + 1)),
            );
        }
        Ok(())
    }
}

fn validate_name(name: &str, flavor: PathFlavor) -> Result<(), String> {
    if name.is_empty()
        || matches!(name, "." | "..")
        || name.len() > MAX_NODE_NAME_BYTES
        || name.chars().count() > MAX_NODE_NAME_CHARS
        || name.contains(['\0', '/'])
    {
        return Err("invalid filename component or filename budget exceeded".into());
    }
    if flavor == PathFlavor::Windows
        && name
            .chars()
            .any(|c| c < '\u{20}' || "\\:*?\"<>|".contains(c))
    {
        return Err("invalid Windows filename component".into());
    }
    Ok(())
}
fn normalize_root(path: &str, flavor: PathFlavor) -> Result<String, ChangeError> {
    let error = || {
        ChangeError::InvalidState(
            "root must be an explicit local absolute lexical directory without '.' or '..'".into(),
        )
    };
    if path.is_empty() || path.len() > 32768 || path.contains('\0') {
        return Err(error());
    }
    match flavor {
        PathFlavor::Posix => {
            if !path.starts_with('/') || path.starts_with("//") {
                return Err(error());
            }
            let normalized = path.trim_end_matches('/');
            if normalized.is_empty() {
                return Ok("/".into());
            }
            for component in normalized[1..].split('/') {
                validate_name(component, flavor).map_err(|_| error())?;
            }
            Ok(normalized.into())
        }
        PathFlavor::Windows => {
            let path = path.replace('/', "\\");
            let bytes = path.as_bytes();
            if bytes.len() < 3
                || !bytes[0].is_ascii_alphabetic()
                || bytes[1] != b':'
                || bytes[2] != b'\\'
            {
                return Err(error());
            }
            let mut normalized = path[..1].to_ascii_uppercase() + &path[1..];
            while normalized.len() > 3 && normalized.ends_with('\\') {
                normalized.pop();
            }
            if normalized.len() > 3 {
                for component in normalized[3..].split('\\') {
                    validate_name(component, flavor).map_err(|_| error())?;
                }
            }
            Ok(normalized)
        }
    }
}

// Stable wire format: nodes are a sequence, so duplicate identities can be
// rejected during restoration instead of silently overwritten by JSON maps.
struct NodeSequence<'a>(&'a BTreeMap<FileId, FileNode>);
impl Serialize for NodeSequence<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for node in self.0.values() {
            sequence.serialize_element(node)?;
        }
        sequence.end()
    }
}
impl Serialize for JournalState {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut structure = serializer.serialize_struct("JournalState", 7)?;
        structure.serialize_field("schema_version", &SCHEMA_VERSION)?;
        structure.serialize_field("journal_id", &self.journal_id)?;
        structure.serialize_field("cursor", &self.cursor)?;
        structure.serialize_field("root_id", &self.root_id)?;
        structure.serialize_field("root_path", &self.root_path)?;
        structure.serialize_field("flavor", &self.flavor)?;
        structure.serialize_field("nodes", &NodeSequence(&self.nodes))?;
        structure.end()
    }
}
impl<'de> Deserialize<'de> for JournalState {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            schema_version: u32,
            journal_id: u64,
            cursor: i64,
            root_id: FileId,
            root_path: String,
            flavor: PathFlavor,
            nodes: Vec<FileNode>,
        }
        let wire = Wire::deserialize(deserializer)?;
        if wire.schema_version != SCHEMA_VERSION {
            return Err(D::Error::custom("unsupported journal graph schema"));
        }
        if wire.nodes.len() > MAX_GRAPH_NODES {
            return Err(D::Error::custom("journal graph exceeds node budget"));
        }
        let mut state = Self::new(
            wire.journal_id,
            wire.cursor,
            wire.root_id,
            wire.root_path,
            wire.flavor,
        )
        .map_err(D::Error::custom)?;
        for node in wire.nodes {
            if state.nodes.insert(node.id, node).is_some() {
                return Err(D::Error::custom("duplicate file identity in journal graph"));
            }
        }
        state.rebuild_and_validate().map_err(D::Error::custom)?;
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn id(value: u64) -> FileId {
        value.into()
    }
    fn node(value: u64, parent: u64, name: &str, is_dir: bool) -> FileNode {
        FileNode {
            id: id(value),
            parent_id: id(parent),
            name: name.into(),
            is_dir,
            size: 1,
            modified: 0,
        }
    }
    fn state(flavor: PathFlavor) -> JournalState {
        JournalState::new(
            77,
            0,
            id(1),
            if flavor == PathFlavor::Windows {
                "C:\\"
            } else {
                "/fixture"
            }
            .into(),
            flavor,
        )
        .unwrap()
    }
    fn batch(start: i64, changes: Vec<Change>) -> JournalBatch {
        let end = start + changes.len() as i64 + 1;
        JournalBatch {
            journal_id: 77,
            start_cursor: start,
            end_cursor: end,
            events: changes
                .into_iter()
                .enumerate()
                .map(|(i, change)| ChangeEvent {
                    usn: start + i as i64,
                    change,
                })
                .collect(),
        }
    }
    fn populated(flavor: PathFlavor) -> JournalState {
        let mut state = state(flavor);
        state
            .apply_batch(&batch(
                0,
                vec![
                    Change::Upsert {
                        node: node(2, 1, "docs", true),
                    },
                    Change::Upsert {
                        node: node(3, 2, "report.txt", false),
                    },
                    Change::Upsert {
                        node: node(4, 2, "nested", true),
                    },
                    Change::Upsert {
                        node: node(5, 4, "child.pdf", false),
                    },
                ],
            ))
            .unwrap();
        state
    }
    fn apply(state: &mut JournalState, changes: Vec<Change>) -> ApplyOutcome {
        state.apply_batch(&batch(state.cursor(), changes)).unwrap()
    }

    #[test]
    fn stable_id_upsert_never_duplicates_and_complete_batch_replay_is_noop() {
        let mut state = state(PathFlavor::Posix);
        let changes = batch(
            0,
            vec![Change::Upsert {
                node: node(2, 1, "a.txt", false),
            }],
        );
        state.apply_batch(&changes).unwrap();
        let snapshot = state.clone();
        assert_eq!(
            state.apply_batch(&changes).unwrap(),
            ApplyOutcome::AlreadyApplied { cursor: 2 }
        );
        assert_eq!(state, snapshot);
        let outcome = apply(
            &mut state,
            vec![Change::Upsert {
                node: node(2, 1, "a.txt", false),
            }],
        );
        assert!(matches!(outcome, ApplyOutcome::Applied { upserted: 0, .. }));
        assert_eq!(state.len(), 1);
    }

    #[test]
    fn rename_across_batches_replaces_the_same_id_without_old_name_record_dependency() {
        let mut state = populated(PathFlavor::Windows);
        apply(&mut state, vec![Change::RenameOld { id: id(3) }]);
        assert_eq!(
            state.resolve_path(id(3)).unwrap().as_deref(),
            Some("C:\\docs\\report.txt")
        );
        apply(
            &mut state,
            vec![Change::Rename {
                id: id(3),
                parent_id: id(2),
                name: "final.txt".into(),
            }],
        );
        assert_eq!(
            state.resolve_path(id(3)).unwrap().as_deref(),
            Some("C:\\docs\\final.txt")
        );
        assert_eq!(state.len(), 4);
        assert!(!state.iter().any(|node| node.name == "report.txt"));
        apply(
            &mut state,
            vec![Change::Upsert {
                node: node(3, 2, "next.txt", false),
            }],
        );
        assert_eq!(
            state.resolve_path(id(3)).unwrap().as_deref(),
            Some("C:\\docs\\next.txt")
        );
    }

    #[test]
    fn directory_rename_updates_every_descendant_path_from_stable_parent_ids() {
        for flavor in [PathFlavor::Windows, PathFlavor::Posix] {
            let mut state = populated(flavor);
            apply(
                &mut state,
                vec![Change::Rename {
                    id: id(2),
                    parent_id: id(1),
                    name: "renamed".into(),
                }],
            );
            let expected = if flavor == PathFlavor::Windows {
                "C:\\renamed\\nested\\child.pdf"
            } else {
                "/fixture/renamed/nested/child.pdf"
            };
            assert_eq!(
                state.resolve_path(id(5)).unwrap().as_deref(),
                Some(expected)
            );
            assert_eq!(state.get(id(5)).unwrap().parent_id, id(4));
        }
    }

    #[test]
    fn append_and_move_resolve_current_paths_without_separate_stale_path_tape() {
        let mut state = populated(PathFlavor::Posix);
        apply(
            &mut state,
            vec![Change::Upsert {
                node: node(6, 4, "new.txt", false),
            }],
        );
        assert_eq!(
            state.resolve_path(id(6)).unwrap().as_deref(),
            Some("/fixture/docs/nested/new.txt")
        );
        apply(
            &mut state,
            vec![Change::Rename {
                id: id(6),
                parent_id: id(1),
                name: "new.txt".into(),
            }],
        );
        assert_eq!(
            state.resolve_path(id(6)).unwrap().as_deref(),
            Some("/fixture/new.txt")
        );
    }

    #[test]
    fn deleting_parent_invalidates_whole_subtree_without_child_events() {
        let mut state = populated(PathFlavor::Posix);
        let outcome = apply(&mut state, vec![Change::Delete { id: id(2) }]);
        assert!(matches!(outcome, ApplyOutcome::Applied { removed: 4, .. }));
        assert!(state.is_empty());
        assert_eq!(state.resolve_path(id(5)).unwrap(), None);
        // Reusing a parent ID cannot resurrect old descendants from stale slots.
        apply(
            &mut state,
            vec![Change::Upsert {
                node: node(2, 1, "recreated", true),
            }],
        );
        assert_eq!(state.len(), 1);
        assert_eq!(state.resolve_path(id(5)).unwrap(), None);
    }

    #[test]
    fn moved_child_survives_old_parent_deletion_in_the_same_batch() {
        let mut state = populated(PathFlavor::Posix);
        apply(
            &mut state,
            vec![
                Change::Rename {
                    id: id(5),
                    parent_id: id(1),
                    name: "moved.pdf".into(),
                },
                Change::Delete { id: id(2) },
            ],
        );
        assert_eq!(state.len(), 1);
        assert_eq!(
            state.resolve_path(id(5)).unwrap().as_deref(),
            Some("/fixture/moved.pdf")
        );
    }

    #[test]
    fn newly_added_descendants_are_included_in_same_batch_deletion() {
        let mut state = populated(PathFlavor::Posix);
        apply(
            &mut state,
            vec![
                Change::Upsert {
                    node: node(6, 4, "new.txt", false),
                },
                Change::Delete { id: id(2) },
            ],
        );
        assert!(state.is_empty());
    }

    #[test]
    fn unknown_parent_is_hidden_until_resolved_and_never_fabricated_at_root() {
        let mut state = state(PathFlavor::Posix);
        apply(
            &mut state,
            vec![Change::Upsert {
                node: node(3, 99, "orphan.txt", false),
            }],
        );
        assert_eq!(state.len(), 1);
        assert_eq!(state.resolve_path(id(3)).unwrap(), None);
        apply(
            &mut state,
            vec![Change::Upsert {
                node: node(99, 1, "late-parent", true),
            }],
        );
        assert_eq!(
            state.resolve_path(id(3)).unwrap().as_deref(),
            Some("/fixture/late-parent/orphan.txt")
        );
    }

    #[test]
    fn deleting_unknown_parent_also_removes_its_known_orphan_subtree() {
        let mut state = state(PathFlavor::Posix);
        apply(
            &mut state,
            vec![Change::Upsert {
                node: node(3, 99, "orphan.txt", false),
            }],
        );
        apply(&mut state, vec![Change::Delete { id: id(99) }]);
        assert!(state.is_empty());
    }

    #[test]
    fn journal_recreation_gap_and_overlap_require_explicit_rebuild() {
        let mut state = populated(PathFlavor::Posix);
        let original = state.clone();
        let mut replaced = batch(state.cursor(), vec![]);
        replaced.journal_id += 1;
        assert!(matches!(
            state.apply_batch(&replaced),
            Err(ChangeError::RebuildRequired(
                RebuildReason::JournalChanged { .. }
            ))
        ));
        let gap = batch(state.cursor() + 1, vec![]);
        assert!(matches!(
            state.apply_batch(&gap),
            Err(ChangeError::RebuildRequired(
                RebuildReason::CursorGap { .. }
            ))
        ));
        let overlap = batch(state.cursor() - 1, vec![Change::RenameOld { id: id(3) }]);
        assert!(matches!(
            state.apply_batch(&overlap),
            Err(ChangeError::RebuildRequired(
                RebuildReason::CursorOverlap { .. }
            ))
        ));
        assert_eq!(state, original);
    }

    #[test]
    fn graph_and_cursor_rollback_after_late_invalid_event() {
        let mut state = populated(PathFlavor::Posix);
        let original = state.clone();
        let changes = batch(
            state.cursor(),
            vec![
                Change::Rename {
                    id: id(3),
                    parent_id: id(1),
                    name: "would-change.txt".into(),
                },
                Change::Upsert {
                    node: node(2, 4, "docs", true),
                }, // cycle through 4 -> 2
            ],
        );
        assert!(matches!(
            state.apply_batch(&changes),
            Err(ChangeError::InvalidBatch(_))
        ));
        assert_eq!(state, original);
    }

    #[test]
    fn file_cannot_be_used_as_parent_or_replace_nonempty_directory() {
        let mut state = populated(PathFlavor::Posix);
        let original = state.clone();
        assert!(state
            .apply_batch(&batch(
                state.cursor(),
                vec![Change::Upsert {
                    node: node(6, 3, "child", false)
                }]
            ))
            .is_err());
        assert!(state
            .apply_batch(&batch(
                state.cursor(),
                vec![Change::Upsert {
                    node: node(2, 1, "file", false)
                }]
            ))
            .is_err());
        assert_eq!(state, original);
        apply(
            &mut state,
            vec![
                Change::Delete { id: id(2) },
                Change::Upsert {
                    node: node(2, 1, "file", false),
                },
            ],
        );
        assert_eq!(state.len(), 1);
    }

    #[test]
    fn invalid_envelopes_and_names_never_advance_cursor() {
        let mut state = populated(PathFlavor::Windows);
        let original = state.clone();
        for name in ["..", "a\\b", "a/b", "x:stream", "", "nul\0"] {
            assert!(state
                .apply_batch(&batch(
                    state.cursor(),
                    vec![Change::Upsert {
                        node: node(8, 1, name, false)
                    }]
                ))
                .is_err());
            assert_eq!(state, original);
        }
        let mut invalid = batch(state.cursor(), vec![Change::RenameOld { id: id(3) }]);
        invalid.events[0].usn = invalid.end_cursor;
        assert!(state.apply_batch(&invalid).is_err());
        invalid.events[0].usn = invalid.start_cursor;
        invalid.events.push(invalid.events[0].clone());
        assert!(state.apply_batch(&invalid).is_err());
        assert_eq!(state, original);
    }

    #[test]
    fn root_changes_and_rename_without_metadata_request_rebuild() {
        let mut state = populated(PathFlavor::Posix);
        let original = state.clone();
        assert!(matches!(
            state.apply_batch(&batch(state.cursor(), vec![Change::Delete { id: id(1) }])),
            Err(ChangeError::RebuildRequired(RebuildReason::RootChanged))
        ));
        assert!(matches!(
            state.apply_batch(&batch(
                state.cursor(),
                vec![Change::Rename {
                    id: id(999),
                    parent_id: id(1),
                    name: "unknown".into()
                }]
            )),
            Err(ChangeError::RebuildRequired(
                RebuildReason::MissingRenameMetadata { .. }
            ))
        ));
        assert_eq!(state, original);
    }

    #[test]
    fn stable_identity_and_graph_json_roundtrip_validate_derived_state() {
        let mut state = populated(PathFlavor::Windows);
        let large = FileId(u128::MAX - 4);
        let mut file = node(8, 1, "large-id.txt", false);
        file.id = large;
        apply(&mut state, vec![Change::Upsert { node: file }]);
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("fffffffffffffffffffffffffffffffb"));
        let mut restored: JournalState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, state);
        apply(&mut restored, vec![Change::Delete { id: id(2) }]);
        assert_eq!(restored.len(), 1);
        assert!(restored.resolve_path(large).unwrap().is_some());
        let mut bad = serde_json::to_value(state).unwrap();
        let duplicate = bad["nodes"][0].clone();
        bad["nodes"].as_array_mut().unwrap().push(duplicate);
        assert!(serde_json::from_value::<JournalState>(bad).is_err());
    }

    #[test]
    fn corrupted_snapshot_cycle_and_unknown_fields_are_rejected() {
        let state = populated(PathFlavor::Posix);
        let mut bad = serde_json::to_value(&state).unwrap();
        bad["nodes"][0]["parent_id"] = serde_json::to_value(id(4)).unwrap();
        assert!(serde_json::from_value::<JournalState>(bad).is_err());
        let mut bad = serde_json::to_value(&state).unwrap();
        bad["unknown"] = serde_json::json!(true);
        assert!(serde_json::from_value::<JournalState>(bad).is_err());
        assert!(serde_json::from_str::<FileId>("9007199254740993").is_err());
    }

    #[test]
    fn host_independent_paths_preserve_posix_backslashes_and_no_network_roots() {
        let mut state = state(PathFlavor::Posix);
        apply(
            &mut state,
            vec![Change::Upsert {
                node: node(2, 1, "literal\\name", false),
            }],
        );
        assert_eq!(
            state.resolve_path(id(2)).unwrap().as_deref(),
            Some("/fixture/literal\\name")
        );
        for (root, flavor) in [
            ("\\\\server\\share", PathFlavor::Windows),
            ("//server/share", PathFlavor::Posix),
            ("C:relative", PathFlavor::Windows),
            ("/a/../b", PathFlavor::Posix),
            ("C:\\a\\..\\b", PathFlavor::Windows),
        ] {
            assert!(JournalState::new(77, 0, id(1), root.into(), flavor).is_err());
        }
    }
    #[test]
    fn moving_directory_cannot_hide_an_overdeep_unchanged_descendant() {
        let mut state = state(PathFlavor::Posix);
        let mut changes = Vec::new();
        for value in 2..=MAX_GRAPH_DEPTH as u64 + 1 {
            changes.push(Change::Upsert {
                node: node(value, value - 1, "d", true),
            });
        }
        apply(&mut state, changes);
        let original = state.clone();
        let moves = batch(
            state.cursor(),
            vec![
                Change::Upsert {
                    node: node(90_000, 1, "new-parent", true),
                },
                Change::Rename {
                    id: id(2),
                    parent_id: id(90_000),
                    name: "d".into(),
                },
            ],
        );
        assert!(matches!(
            state.apply_batch(&moves),
            Err(ChangeError::InvalidBatch(_))
        ));
        assert_eq!(state, original);
        let snapshot = serde_json::to_string(&state).unwrap();
        assert_eq!(
            serde_json::from_str::<JournalState>(&snapshot).unwrap(),
            state
        );
    }
}
