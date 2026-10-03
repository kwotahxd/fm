//! Shared domain types and the wire types of the AI IPC protocol.
//! No logic beyond trivial helpers: every other crate depends on this one, and only on this one
//! for cross-module data exchange, which is what keeps the modules independent.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

// ───────────────────────── index records ─────────────────────────

/// A file as seen by the index (DB row + tags).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FileRecord {
    pub id: i64,
    pub path: String,
    pub name: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub mode: u32,
    pub mime: Option<String>,
    pub hash: Option<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub summary: Option<String>,
    pub attrs: BTreeMap<String, String>,
    /// unix seconds of the last AI analysis
    pub analyzed_at: Option<i64>,
    /// file mtime at the moment of analysis: `!= mtime_ns` means the analysis is stale
    pub analyzed_mtime_ns: Option<i64>,
    pub indexed_at: i64,
}

impl FileRecord {
    pub fn ext(&self) -> Option<String> {
        Path::new(&self.name)
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
    }
    pub fn stem(&self) -> String {
        Path::new(&self.name)
            .file_stem()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.name.clone())
    }
    pub fn needs_analysis(&self) -> bool {
        self.analyzed_at.is_none() || self.analyzed_mtime_ns != Some(self.mtime_ns)
    }
}

/// What the walker hands to the DB layer.
#[derive(Debug, Clone)]
pub struct NewFile {
    pub path: String,
    pub name: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub mode: u32,
    pub mime: Option<String>,
}

// ───────────────────────── AI protocol ─────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AttrSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnalyzeRequest {
    pub path: String,
    #[serde(default)]
    pub mime: Option<String>,
    /// existing categories: the model is told to reuse them when they fit
    #[serde(default)]
    pub categories: Vec<String>,
    /// extra per-file attributes to extract (from a sorting rule)
    #[serde(default)]
    pub attributes: Vec<AttrSpec>,
    #[serde(default)]
    pub want_embedding: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnalysisResult {
    pub category: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,
    #[serde(default)]
    pub embedding_model: Option<String>,
    /// model that produced the classification
    #[serde(default)]
    pub model: String,
    /// MIME refined by the engine (magic bytes), if it differs from the extension guess
    #[serde(default)]
    pub mime: Option<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EmbedResponse {
    pub model: String,
    pub vectors: Vec<Vec<f32>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub version: String,
    pub backend: String,
    pub ollama_reachable: bool,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub missing_models: Vec<String>,
}

// ───────────────────────── sorting rules ─────────────────────────

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Conflict {
    /// `name (1).ext`, `name (2).ext`, …
    #[default]
    Rename,
    Skip,
    Fail,
}

impl std::str::FromStr for Conflict {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "rename" => Ok(Self::Rename),
            "skip" => Ok(Self::Skip),
            "fail" => Ok(Self::Fail),
            o => Err(format!("unknown conflict strategy '{o}' (rename|skip|fail)")),
        }
    }
}

/// Which files a rule applies to. Empty field = no constraint; all set fields must match.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Filter {
    pub mime_prefix: Vec<String>,
    pub ext: Vec<String>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub category_in: Vec<String>,
    pub tag_any: Vec<String>,
    /// AI attributes that must equal the given value (case-insensitive)
    pub attr_equals: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Rule {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub filter: Filter,
    /// Destination *directory* template relative to the sort root, e.g. `Photos/{year}/{attr.event}`.
    pub dest: String,
    /// Optional new file name template (default: keep the name), e.g. `{year}-{month}-{day}_{stem}.{ext}`.
    #[serde(default)]
    pub rename: Option<String>,
}

/// An ordered list of rules; the first matching rule wins. Files matching no rule stay where they are.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RuleSet {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// per-file attributes the AI must extract for this rule set
    #[serde(default)]
    pub attributes: Vec<AttrSpec>,
    pub rules: Vec<Rule>,
}

// ───────────────────────── sort history ─────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OpStatus {
    Applied,
    Undone,
    Failed,
}

impl OpStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Undone => "undone",
            Self::Failed => "failed",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "undone" => Self::Undone,
            "failed" => Self::Failed,
            _ => Self::Applied,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SortOpRecord {
    pub id: i64,
    pub batch_id: String,
    pub seq: u32,
    pub rule_name: Option<String>,
    pub prompt: Option<String>,
    pub root: String,
    pub src: String,
    pub dst: String,
    pub status: OpStatus,
    pub error: Option<String>,
    pub applied_at: i64,
    pub undone_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchSummary {
    pub batch_id: String,
    pub rule_name: Option<String>,
    pub prompt: Option<String>,
    pub root: String,
    pub created_at: i64,
    pub total: u32,
    pub applied: u32,
    pub undone: u32,
    pub failed: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedRule {
    pub name: String,
    pub description: String,
    pub prompt: Option<String>,
    pub source: String,
    pub spec: RuleSet,
    pub created_at: i64,
}
