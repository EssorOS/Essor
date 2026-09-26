//! Shared data types for the plain (non-CRDT) document model.
//!
//! A document is a flat set of [`Record`]s keyed by id and merged by
//! last-write-wins on a Lamport [`Clock`]. Page metadata and block content are
//! both records, distinguished by [`RecordPayload`], so one store holds the
//! whole workspace. The same JSON shapes are used on disk and on the wire.

use serde::{Deserialize, Serialize};

/// A Lamport clock stamp: a per-workspace counter plus the authoring client id.
///
/// Ordering is the tuple `(counter, client)`; the client id only breaks ties.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Clock {
    pub c: u64,
    pub client: u64,
}

impl Clock {
    pub fn new(c: u64, client: u64) -> Self {
        Self { c, client }
    }
}

impl PartialOrd for Clock {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Clock {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.c
            .cmp(&other.c)
            .then_with(|| self.client.cmp(&other.client))
    }
}

/// The kind of a block. Kept intentionally small.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BlockKind {
    #[default]
    Paragraph,
    Heading1,
    Heading2,
    Bullet,
}

/// A contiguous run of text sharing the same inline marks.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextRun {
    pub text: String,
    #[serde(default)]
    pub bold: bool,
    #[serde(default)]
    pub italic: bool,
}

/// An inline mark that can be toggled over a range of text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    Bold,
    Italic,
}

/// Stable identity of a block: a UUID, persisted verbatim in the record id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub uuid::Uuid);

impl BlockId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }

    /// The 32-character simple form used as a record id.
    pub fn to_simple(self) -> String {
        self.0.simple().to_string()
    }

    /// Parse a record id back into a `BlockId`.
    pub fn parse(value: &str) -> Option<Self> {
        uuid::Uuid::parse_str(value).ok().map(BlockId)
    }
}

impl Default for BlockId {
    fn default() -> Self {
        Self::new()
    }
}

/// The payload of a record: page metadata or block content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum RecordPayload {
    Page { title: String },
    Block { kind: BlockKind, runs: Vec<TextRun> },
}

impl RecordPayload {
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Page { .. } => "page",
            Self::Block { .. } => "block",
        }
    }

    /// The block's kind and runs, or `None` for a page record.
    pub fn as_block(&self) -> Option<(&BlockKind, &[TextRun])> {
        match self {
            Self::Block { kind, runs } => Some((kind, runs)),
            Self::Page { .. } => None,
        }
    }
}

/// One addressable, independently-versioned record in the workspace.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    /// The page this record belongs to. A page record uses its own id here.
    pub page: String,
    /// Fractional index ordering within the page.
    #[serde(default)]
    pub position: String,
    pub version: Clock,
    #[serde(default)]
    pub deleted: bool,
    #[serde(flatten)]
    pub payload: RecordPayload,
}

impl Record {
    /// The block's kind and runs, or `None` for a page record.
    pub fn as_block(&self) -> Option<(&BlockKind, &[TextRun])> {
        self.payload.as_block()
    }

    /// This record as its tombstone: same identity, page and position, marked
    /// deleted with an empty body. Callers stamp a fresh version so the delete
    /// wins last-write-wins.
    pub fn tombstone(&self) -> Record {
        let payload = match &self.payload {
            RecordPayload::Block { kind, .. } => RecordPayload::Block {
                kind: *kind,
                runs: Vec::new(),
            },
            RecordPayload::Page { .. } => RecordPayload::Page {
                title: String::new(),
            },
        };
        Record {
            id: self.id.clone(),
            page: self.page.clone(),
            position: self.position.clone(),
            version: self.version,
            deleted: true,
            payload,
        }
    }
}
