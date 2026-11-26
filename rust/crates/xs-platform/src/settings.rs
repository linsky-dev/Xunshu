use crate::local::{atomic_write, normalize_path, read_local_file};
use crate::{PlatformError, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;

pub const SETTINGS_VERSION: u32 = 1;
const MAX_SETTINGS_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SetupState {
    #[default]
    NotStarted,
    Deferred,
    Completed,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SpeedMode {
    #[default]
    Standard,
    FastService,
    OnDemandElevated,
}
impl fmt::Display for SpeedMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Standard => "standard",
            Self::FastService => "fast_service",
            Self::OnDemandElevated => "on_demand_elevated",
        })
    }
}
fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub path: String,
    /// null = all file types; [] = directories only; otherwise a strict whitelist.
    #[serde(default)]
    pub extensions: Option<Vec<String>>,
    #[serde(default = "default_true")]
    pub smart_excludes: bool,
}
impl Scope {
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            extensions: None,
            smart_excludes: true,
        }
    }
    pub(crate) fn normalized(&self) -> Result<Self> {
        let path = normalize_path(Path::new(&self.path))?;
        let extensions = match &self.extensions {
            None => None,
            Some(v) => {
                if v.len() > 256 {
                    return Err(PlatformError::InvalidSettings(
                        "At most 256 extensions are allowed per scope".into(),
                    ));
                }
                let mut normalized = BTreeSet::new();
                for ext in v {
                    let value = ext.strip_prefix('.').unwrap_or(ext).to_ascii_lowercase();
                    if value.is_empty()
                        || value.len() > 32
                        || !value.as_bytes()[0].is_ascii_alphanumeric()
                        || !value
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'+' | b'-'))
                    {
                        return Err(PlatformError::InvalidSettings(format!(
                            "Invalid extension {ext:?}; use an extension such as txt, without wildcards or separators"
                        )));
                    }
                    normalized.insert(value);
                }
                Some(normalized.into_iter().collect())
            }
        };
        Ok(Self {
            path: path
                .to_str()
                .ok_or_else(|| PlatformError::InvalidPath("Path is not UTF-8".into()))?
                .to_owned(),
            extensions,
            smart_excludes: self.smart_excludes,
        })
    }
    pub(crate) fn accepts(&self, path: &Path, is_dir: bool) -> bool {
        if is_dir {
            return true;
        }
        match &self.extensions {
            None => true,
            Some(exts) => path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|ext| exts.iter().any(|s| s.eq_ignore_ascii_case(ext))),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub version: u32,
    #[serde(default)]
    pub setup_state: SetupState,
    #[serde(default)]
    pub reminder_count: u8,
    #[serde(default)]
    pub mode: SpeedMode,
    #[serde(default)]
    pub roots: Vec<Scope>,
    #[serde(default)]
    pub content_roots: Vec<Scope>,
    #[serde(default)]
    pub content_enabled: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub content_follows_roots: bool,
    #[serde(default)]
    pub ocr_enabled: bool,
    #[serde(default)]
    pub semantic_enabled: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            setup_state: SetupState::NotStarted,
            reminder_count: 0,
            mode: SpeedMode::Standard,
            roots: Vec::new(),
            content_roots: Vec::new(),
            content_enabled: false,
            content_follows_roots: false,
            ocr_enabled: false,
            semantic_enabled: false,
        }
    }
}
impl Settings {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let bytes = read_local_file(path.as_ref(), MAX_SETTINGS_BYTES)?;
        Self::decode(&bytes)
    }

    /// 读取已经过本地边界验证的文件句柄，保证解析与版本检查针对同一对象。
    pub fn load_open_file(file: &mut File) -> Result<Self> {
        let mut bytes = Vec::new();
        file.take(MAX_SETTINGS_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|source| crate::local::io_error(Path::new("<设置文件句柄>"), source))?;
        if bytes.len() > MAX_SETTINGS_BYTES {
            return Err(PlatformError::InvalidSettings(
                "设置文件超出大小限制".into(),
            ));
        }
        Self::decode(&bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        // The preliminary value only selects the schema. Typed reparse below
        // still rejects duplicate fields, wrong types and unknown keys.
        let value: serde_json::Value = serde_json::from_slice(bytes)?;
        validate_structure(&value, 0)?;
        let result = if value.get("setup_wizard_completed").is_some() {
            let old: LegacySettings = serde_json::from_slice(bytes)?;
            Self {
                setup_state: if old.setup_wizard_completed {
                    SetupState::Completed
                } else {
                    SetupState::NotStarted
                },
                ..Self::default()
            }
        } else {
            serde_json::from_slice::<Self>(bytes)?
        };
        result.normalized()
    }
    pub fn save(&self, path: impl AsRef<Path>) -> Result<crate::LocalWriteOutcome> {
        let value = self.normalized()?;
        let mut bytes = serde_json::to_vec_pretty(&value)?;
        bytes.push(b'\n');
        let path = normalize_path(path.as_ref())?;
        crate::create_local_dir_all(
            path.parent()
                .ok_or_else(|| PlatformError::InvalidPath("File parent required".into()))?,
        )?;
        atomic_write(&path, &bytes, true)
    }
    /// Migrates JSON to a new destination. Existing destination and source alias
    /// are refused. Roots remain empty because legacy root policy lives in SQLite;
    /// importing that policy requires a separate, explicitly validated operation.
    pub fn migrate_legacy(
        old_path: impl AsRef<Path>,
        new_path: impl AsRef<Path>,
    ) -> Result<(Self, crate::LocalWriteOutcome)> {
        let old = normalize_path(old_path.as_ref())?;
        let new = normalize_path(new_path.as_ref())?;
        if same_path(&old, &new) {
            return Err(PlatformError::InvalidSettings(
                "Migration destination must differ from the original settings file".into(),
            ));
        }
        let migrated = Self::load(&old)?;
        let mut bytes = serde_json::to_vec_pretty(&migrated)?;
        bytes.push(b'\n');
        crate::create_local_dir_all(
            new.parent()
                .ok_or_else(|| PlatformError::InvalidPath("File parent required".into()))?,
        )?;
        let outcome = atomic_write(&new, &bytes, false)?;
        Ok((migrated, outcome))
    }
    pub fn validate(&self) -> Result<()> {
        self.normalized().map(|_| ())
    }
    pub(crate) fn normalized(&self) -> Result<Self> {
        if self.version != SETTINGS_VERSION {
            return Err(PlatformError::InvalidSettings(format!(
                "Unsupported settings version {}",
                self.version
            )));
        }
        if self.reminder_count > 3 {
            return Err(PlatformError::InvalidSettings(
                "Reminder count must be between 0 and 3".into(),
            ));
        }
        let roots = normalize_scopes(&self.roots)?;
        let content_roots = if self.content_follows_roots {
            roots.clone()
        } else {
            normalize_scopes(&self.content_roots)?
        };
        Ok(Self {
            roots,
            content_roots,
            ..self.clone()
        })
    }
    /// A stable, collision-free serialized authorization token for filename
    /// indexing. Content scope and enhancement preferences deliberately do not
    /// change it. A token mismatch means the previous name snapshot is stale.
    pub fn scope_token(&self) -> Result<String> {
        let normal = self.normalized()?;
        #[derive(Serialize)]
        struct Token<'a> {
            version: u32,
            mode: SpeedMode,
            roots: &'a [Scope],
        }
        Ok(serde_json::to_string(&Token {
            version: normal.version,
            mode: normal.mode,
            roots: &normal.roots,
        })?)
    }
    /// 正文的范围与功能授权代次，独立于文件名索引。
    pub fn content_scope_token(&self) -> Result<String> {
        let normal = self.normalized()?;
        Ok(serde_json::to_string(&(
            normal.version,
            normal.content_enabled,
            normal.ocr_enabled,
            normal.semantic_enabled,
            normal.content_roots,
        ))?)
    }
    pub fn should_remind(&self) -> bool {
        self.setup_state != SetupState::Completed && self.reminder_count < 3
    }
    pub fn record_reminder(&mut self) -> bool {
        if !self.should_remind() {
            false
        } else {
            self.reminder_count += 1;
            true
        }
    }
    pub fn defer_setup(&mut self) {
        self.setup_state = SetupState::Deferred;
    }
    pub fn complete_setup(&mut self) {
        if self.setup_state != SetupState::Completed
            && !self.roots.is_empty()
            && !self.content_enabled
            && self.content_roots.is_empty()
        {
            self.content_enabled = true;
            self.content_follows_roots = true;
            self.content_roots = self.roots.clone();
        }
        self.setup_state = SetupState::Completed;
    }
    pub fn reopen_setup(&mut self) {
        self.setup_state = SetupState::NotStarted;
    }
    pub fn allows_name(&self, path: &Path, is_dir: bool) -> Result<bool> {
        let p = normalize_path(path)?;
        let s = self.normalized()?;
        Ok(scope_for(&s.roots, &p)
            .is_some_and(|root| root.accepts(&p, is_dir) && !is_smart_excluded(root, &p)))
    }
    pub fn allows_filename(&self, path: &Path, is_dir: bool) -> Result<bool> {
        self.allows_name(path, is_dir)
    }
    pub fn allows_content(&self, path: &Path, is_dir: bool) -> Result<bool> {
        let p = normalize_path(path)?;
        let s = self.normalized()?;
        Ok(s.content_enabled
            && scope_for(&s.content_roots, &p)
                .is_some_and(|root| root.accepts(&p, is_dir) && !is_smart_excluded(root, &p)))
    }
}
fn normalize_scopes(scopes: &[Scope]) -> Result<Vec<Scope>> {
    if scopes.len() > 256 {
        return Err(PlatformError::InvalidSettings(
            "At most 256 roots are allowed".into(),
        ));
    }
    let mut out = Vec::new();
    for scope in scopes {
        let scope = scope.normalized()?;
        if out
            .iter()
            .any(|s: &Scope| same_path(Path::new(&s.path), Path::new(&scope.path)))
        {
            return Err(PlatformError::InvalidSettings(format!(
                "Duplicate root: {}",
                scope.path
            )));
        }
        out.push(scope);
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}
fn validate_structure(value: &serde_json::Value, depth: usize) -> Result<()> {
    if depth > 64 {
        return Err(PlatformError::InvalidSettings(
            "Settings nesting exceeds 64 levels".into(),
        ));
    }
    match value {
        serde_json::Value::Array(v) => {
            if v.len() > 20000 {
                return Err(PlatformError::InvalidSettings(
                    "Settings collection is too large".into(),
                ));
            }
            for v in v {
                validate_structure(v, depth + 1)?;
            }
        }
        serde_json::Value::Object(v) => {
            if v.len() > 20000 {
                return Err(PlatformError::InvalidSettings(
                    "Settings object is too large".into(),
                ));
            }
            for v in v.values() {
                validate_structure(v, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}
#[cfg(windows)]
fn windows_component_eq(a: &std::ffi::OsStr, b: &std::ffi::OsStr) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Globalization::{CompareStringOrdinal, CSTR_EQUAL};
    // Native ordinal comparison includes Windows Unicode case mappings; using
    // ASCII-only folding would let a differently cased data directory evade
    // hard exclusions. Invalid UTF-16 is never converted lossily here.
    let a: Vec<_> = a.encode_wide().collect();
    let b: Vec<_> = b.encode_wide().collect();
    let (Ok(a_len), Ok(b_len)) = (i32::try_from(a.len()), i32::try_from(b.len())) else {
        return false;
    };
    unsafe { CompareStringOrdinal(a.as_ptr(), a_len, b.as_ptr(), b_len, 1) == CSTR_EQUAL }
}
pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    #[cfg(windows)]
    {
        let a: Vec<_> = a.components().collect();
        let b: Vec<_> = b.components().collect();
        a.len() == b.len()
            && a.iter()
                .zip(b)
                .all(|(x, y)| windows_component_eq(x.as_os_str(), y.as_os_str()))
    }
    #[cfg(not(windows))]
    {
        a == b
    }
}
pub(crate) fn is_within(path: &Path, root: &Path) -> bool {
    #[cfg(windows)]
    {
        let a: Vec<_> = path.components().collect();
        let b: Vec<_> = root.components().collect();
        a.len() >= b.len()
            && a.iter()
                .zip(b)
                .all(|(x, y)| windows_component_eq(x.as_os_str(), y.as_os_str()))
    }
    #[cfg(not(windows))]
    {
        path.starts_with(root)
    }
}
pub(crate) fn scope_for<'a>(scopes: &'a [Scope], path: &Path) -> Option<&'a Scope> {
    scopes
        .iter()
        .filter(|s| is_within(path, Path::new(&s.path)))
        .max_by_key(|s| Path::new(&s.path).components().count())
}
pub(crate) fn is_smart_excluded(scope: &Scope, path: &Path) -> bool {
    if !scope.smart_excludes {
        return false;
    }
    // Applied below the explicitly selected root only. A separately selected
    // deeper root has its own policy and is visited separately.
    let skip = Path::new(&scope.path).components().count();
    path.components().skip(skip).any(|c| {
        let s = c.as_os_str().to_string_lossy();
        [
            ".git",
            ".svn",
            ".hg",
            "node_modules",
            "__pycache__",
            ".pytest_cache",
            ".mypy_cache",
            ".cache",
            "$recycle.bin",
            "system volume information",
        ]
        .iter()
        .any(|name| s.eq_ignore_ascii_case(name))
    })
}

// Every legacy top-level key is explicit. Ignored preferences remain preserved in
// the original file, and cannot silently widen the new indexing authorization.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct LegacySettings {
    setup_wizard_completed: bool,
    #[serde(default)]
    history: Option<serde_json::Value>,
    #[serde(default)]
    bookmarks: Option<serde_json::Value>,
    #[serde(default)]
    geometry: Option<serde_json::Value>,
    #[serde(default)]
    preview: Option<serde_json::Value>,
    #[serde(default)]
    rescan_interval: Option<serde_json::Value>,
    #[serde(default)]
    search_options: Option<serde_json::Value>,
    #[serde(default)]
    last_query: Option<serde_json::Value>,
    #[serde(default)]
    tray_enabled: Option<serde_json::Value>,
    #[serde(default)]
    hide_to_tray: Option<serde_json::Value>,
    #[serde(default)]
    source_filter: Option<serde_json::Value>,
    #[serde(default)]
    result_columns: Option<serde_json::Value>,
    #[serde(default)]
    quick_pinned: Option<serde_json::Value>,
    #[serde(default)]
    quick_recent: Option<serde_json::Value>,
    #[serde(default)]
    quick_remember: Option<serde_json::Value>,
    #[serde(default)]
    global_double_ctrl: Option<serde_json::Value>,
    #[serde(default)]
    auto_index: Option<serde_json::Value>,
    #[serde(default)]
    enhancement_bundle: Option<serde_json::Value>,
    #[serde(default)]
    start_in_tray: Option<serde_json::Value>,
    #[serde(default)]
    window_mode_version: Option<serde_json::Value>,
    #[serde(default)]
    autostart: Option<serde_json::Value>,
}
