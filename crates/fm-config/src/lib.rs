//! TOML configuration. The Python AI engine reads the very same file (`[ai]` section),
//! so there is exactly one source of truth for socket path, models and Ollama URL.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config {0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("invalid config {0}: {1}")]
    Parse(PathBuf, toml::de::Error),
    #[error("invalid config value: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub general: General,
    pub database: DatabaseCfg,
    pub core: CoreCfg,
    pub index: IndexCfg,
    pub ai: AiCfg,
    pub sort: SortCfg,
    pub watch: WatchCfg,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    /// tracing filter, e.g. "info" or "fm_core=debug,info". `RUST_LOG` overrides it.
    pub log_level: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DatabaseCfg {
    /// "sqlite" (implemented) | "postgres" (interface only, see docs)
    pub backend: String,
    pub path: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CoreCfg {
    /// worker threads for directory walking (0 = number of CPUs)
    pub threads: usize,
    /// getdents64 buffer size in KiB
    pub read_buffer_kb: usize,
    pub cache_entries: usize,
    pub cache_ttl_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IndexCfg {
    pub roots: Vec<String>,
    /// exact directory/file names to skip anywhere in the tree
    pub blacklist_names: Vec<String>,
    /// simple globs (`*`, `?`) matched against names
    pub blacklist_globs: Vec<String>,
    /// absolute path prefixes to skip
    pub blacklist_paths: Vec<String>,
    pub skip_hidden: bool,
    /// files larger than this are indexed but not hashed / not sent to the AI (MiB)
    pub max_analyze_mb: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AiCfg {
    pub enabled: bool,
    pub socket_path: String,
    pub ollama_url: String,
    /// model for text classification and prompt parsing
    pub text_model: String,
    /// multimodal model for images
    pub vision_model: String,
    pub embed_model: String,
    pub request_timeout_s: u64,
    /// parallel analysis requests in flight
    pub concurrency: usize,
    /// bytes of file content fed to the model
    pub max_text_bytes: usize,
    /// optional faster-whisper model name for audio transcription ("" = disabled)
    pub whisper_model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SortCfg {
    /// rename | skip | fail
    pub default_conflict: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WatchCfg {
    pub debounce_ms: u64,
    /// analyse changed files automatically while watching (needs AI)
    pub auto_analyze: bool,
}

impl Default for General {
    fn default() -> Self {
        Self { log_level: "info".into() }
    }
}
impl Default for DatabaseCfg {
    fn default() -> Self {
        Self {
            backend: "sqlite".into(),
            path: "~/.local/share/fm/index.db".into(),
            url: String::new(),
        }
    }
}
impl Default for CoreCfg {
    fn default() -> Self {
        Self { threads: 0, read_buffer_kb: 256, cache_entries: 256, cache_ttl_ms: 5000 }
    }
}
impl Default for IndexCfg {
    fn default() -> Self {
        Self {
            roots: vec![],
            blacklist_names: [".git", ".hg", ".svn", "node_modules", "target", "__pycache__", ".cache"]
                .map(String::from)
                .to_vec(),
            blacklist_globs: vec!["*.tmp".into(), "*.swp".into(), "*~".into()],
            blacklist_paths: ["/proc", "/sys", "/dev", "/run"].map(String::from).to_vec(),
            skip_hidden: false,
            max_analyze_mb: 64,
        }
    }
}
impl Default for AiCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            socket_path: String::new(), // empty → default_socket_path()
            ollama_url: "http://127.0.0.1:11434".into(),
            text_model: "llama3".into(),
            vision_model: "llava".into(),
            embed_model: "nomic-embed-text".into(),
            request_timeout_s: 180,
            concurrency: 2,
            max_text_bytes: 6000,
            whisper_model: String::new(),
        }
    }
}
impl Default for SortCfg {
    fn default() -> Self {
        Self { default_conflict: "rename".into() }
    }
}
impl Default for WatchCfg {
    fn default() -> Self {
        Self { debounce_ms: 500, auto_analyze: false }
    }
}

/// `~` and `~/…` expansion.
pub fn expand(p: &str) -> PathBuf {
    if p == "~" || p.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            let mut b = PathBuf::from(home);
            if p.len() > 2 {
                b.push(&p[2..]);
            }
            return b;
        }
    }
    PathBuf::from(p)
}

pub fn default_socket_path() -> PathBuf {
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(rt).join("fm/ai.sock");
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/fm-{uid}/ai.sock"))
}

impl Config {
    /// Lookup order: explicit path → `$FM_CONFIG` → `~/.config/fm/config.toml` → built-in defaults.
    pub fn load(explicit: Option<&Path>) -> Result<Config, ConfigError> {
        let path = if let Some(p) = explicit {
            Some(p.to_path_buf())
        } else if let Some(p) = std::env::var_os("FM_CONFIG") {
            Some(PathBuf::from(p))
        } else {
            let d = expand("~/.config/fm/config.toml");
            d.exists().then_some(d)
        };
        let cfg = match path {
            Some(p) => {
                let s = std::fs::read_to_string(&p).map_err(|e| ConfigError::Io(p.clone(), e))?;
                toml::from_str(&s).map_err(|e| ConfigError::Parse(p.clone(), e))?
            }
            None => Config::default(),
        };
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if !matches!(self.sort.default_conflict.as_str(), "rename" | "skip" | "fail") {
            return Err(ConfigError::Invalid("sort.default_conflict must be rename|skip|fail".into()));
        }
        if !matches!(self.database.backend.as_str(), "sqlite" | "postgres") {
            return Err(ConfigError::Invalid("database.backend must be sqlite|postgres".into()));
        }
        if self.ai.concurrency == 0 {
            return Err(ConfigError::Invalid("ai.concurrency must be ≥ 1".into()));
        }
        Ok(())
    }

    pub fn socket_path(&self) -> PathBuf {
        if self.ai.socket_path.is_empty() {
            default_socket_path()
        } else {
            expand(&self.ai.socket_path)
        }
    }
    pub fn db_path(&self) -> PathBuf {
        expand(&self.database.path)
    }
    pub fn walk_threads(&self) -> usize {
        if self.core.threads == 0 {
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
        } else {
            self.core.threads
        }
    }
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate_and_roundtrip() {
        let c = Config::default();
        c.validate().unwrap();
        let s = c.to_toml();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.ai.text_model, "llama3");
    }

    #[test]
    fn partial_file_keeps_other_defaults() {
        let c: Config = toml::from_str("[ai]\ntext_model = \"mistral\"\n").unwrap();
        assert_eq!(c.ai.text_model, "mistral");
        assert_eq!(c.ai.vision_model, "llava");
        assert_eq!(c.core.read_buffer_kb, 256);
    }

    #[test]
    fn rejects_bad_conflict() {
        let c: Config = toml::from_str("[sort]\ndefault_conflict = \"boom\"\n").unwrap();
        assert!(c.validate().is_err());
    }
}
