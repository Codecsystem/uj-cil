//! Source code compilation, language detection, and compilation caching.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const DEFAULT_LANGUAGES_TOML: &str = include_str!("../../../assets/languages.toml");

#[derive(Debug, thiserror::Error)]
pub enum CompileError {
    #[error("Compilation failed:\n{0}")]
    Failed(String),
    #[error("Compiler '{0}' not found in PATH")]
    CompilerNotFound(String),
    #[error("Unsupported source extension '{0}'")]
    UnsupportedExtension(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct CompileResult {
    pub binary_path: PathBuf,
    pub is_interpreted: bool,
    pub interpreter_cmd: Option<Vec<String>>,
    pub compiler_output: String,
    pub duration_ms: u64,
    pub cached: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Cpp,
    C,
    Rust,
    Python,
    Java,
    Go,
}

impl Language {
    pub fn from_path(path: &Path) -> Result<Self, CompileError> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_lowercase();

        match ext.as_str() {
            "cpp" | "cc" | "cxx" | "c++" => Ok(Self::Cpp),
            "c" => Ok(Self::C),
            "rs" => Ok(Self::Rust),
            "py" => Ok(Self::Python),
            "java" => Ok(Self::Java),
            "go" => Ok(Self::Go),
            other => Err(CompileError::UnsupportedExtension(other.to_string())),
        }
    }

    pub fn is_supported_extension(ext: &str) -> bool {
        matches!(
            ext.to_ascii_lowercase().as_str(),
            "cpp" | "cc" | "cxx" | "c++" | "c" | "rs" | "py" | "java" | "go"
        )
    }

    pub fn id(&self) -> &'static str {
        match self {
            Self::Cpp => "cpp",
            Self::C => "c",
            Self::Rust => "rust",
            Self::Python => "python",
            Self::Java => "java",
            Self::Go => "go",
        }
    }
}

/// A compilation session that caches compiler lookup, `--version` query outputs,
/// merged languages configurations, and problem limits within a command or request.
#[derive(Clone, Default)]
pub struct CompileSession {
    inner: std::sync::Arc<CompileSessionInner>,
}

type FastCompileKey = String;
type PrecheckKey = (PathBuf, Vec<PathBuf>, Vec<PathBuf>);

#[derive(Clone)]
enum CachedOutcome {
    Success(CompileResult),
    Failed(String),
}

#[derive(Clone)]
struct PrecheckEntry {
    /// `(path, mtime, size)` stamps of the main source, all `extra_sources`, all discovered
    /// transitive headers/module dependencies, plus optional `problem.toml` / `languages.toml`.
    stamps: Vec<(PathBuf, std::time::SystemTime, u64)>,
    recorded_at: std::time::SystemTime,
    outcome: CachedOutcome,
}

#[derive(Default)]
struct CompileSessionInner {
    compiler_bins: parking_lot::Mutex<std::collections::HashMap<String, Result<PathBuf, String>>>,
    compiler_versions: parking_lot::Mutex<std::collections::HashMap<PathBuf, String>>,
    languages_configs: parking_lot::Mutex<
        std::collections::HashMap<Option<PathBuf>, uj_core::config::LanguagesConfig>,
    >,
    problem_limits: parking_lot::Mutex<std::collections::HashMap<PathBuf, (u64, u64, u64)>>,
    fast_results: parking_lot::Mutex<std::collections::HashMap<FastCompileKey, CachedOutcome>>,
    mtime_precheck: parking_lot::Mutex<std::collections::HashMap<PrecheckKey, PrecheckEntry>>,
    inflight: parking_lot::Mutex<std::collections::HashMap<String, Arc<parking_lot::Mutex<()>>>>,
}

fn kill_process_tree_by_pid(pid: u32) {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        let p = pid as i32;
        if p > 0 {
            // Child was spawned with process_group(0), so its pgid equals its pid.
            // Signal -pgid with SIGKILL (9) to terminate all descendants.
            unsafe {
                let _ = kill(-p, 9);
            }
        }
    }
    #[cfg(windows)]
    {
        let pid_str = pid.to_string();
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid_str])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn run_command_with_timeout(
    mut cmd: Command,
    timeout: Duration,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>), CompileError> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = cmd.spawn()?;
    let pid = child.id();

    let mut stdout_pipe = child.stdout.take().unwrap();
    let mut stderr_pipe = child.stderr.take().unwrap();

    let (stdout_tx, stdout_rx) = crossbeam_channel::bounded(1);
    let (stderr_tx, stderr_rx) = crossbeam_channel::bounded(1);
    let (wait_tx, wait_rx) = crossbeam_channel::bounded(1);

    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        while let Ok(n) = stdout_pipe.read(&mut chunk) {
            if n == 0 {
                break;
            }
            if buf.len() < 1024 * 1024 {
                buf.extend_from_slice(&chunk[..n]);
            }
        }
        let _ = stdout_tx.send(buf);
    });

    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        while let Ok(n) = stderr_pipe.read(&mut chunk) {
            if n == 0 {
                break;
            }
            if buf.len() < 1024 * 1024 {
                buf.extend_from_slice(&chunk[..n]);
            }
        }
        let _ = stderr_tx.send(buf);
    });

    // Wait directly on the child in a dedicated waiter thread instead of 20ms sleep-polling,
    // waking up immediately upon compiler exit.
    std::thread::spawn(move || {
        let _ = wait_tx.send(child.wait());
    });

    let (timed_out, exit_status) = match wait_rx.recv_timeout(timeout) {
        Ok(res) => (false, Some(res?)),
        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
            kill_process_tree_by_pid(pid);
            let _ = wait_rx.recv_timeout(Duration::from_secs(2));
            (true, None)
        }
        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => (
            false,
            Err(std::io::Error::other(
                "Compiler waiter channel disconnected unexpectedly",
            ))?,
        ),
    };

    let drain_wait = if timed_out {
        Duration::from_millis(200)
    } else {
        Duration::from_secs(2)
    };
    let stdout_bytes = stdout_rx.recv_timeout(drain_wait).unwrap_or_default();
    let stderr_bytes = stderr_rx.recv_timeout(drain_wait).unwrap_or_default();

    if timed_out {
        return Err(CompileError::Failed(format!(
            "Command timed out ({}s limit exceeded)",
            timeout.as_secs()
        )));
    }

    Ok((
        exit_status.expect("status present"),
        stdout_bytes,
        stderr_bytes,
    ))
}

fn scan_directory_recursive(
    dir: &Path,
    extensions: &[&str],
    max_depth: usize,
    out: &mut std::collections::BTreeSet<PathBuf>,
) {
    if max_depth == 0 || !dir.is_dir() {
        return;
    }
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Some(ext) = path.extension().and_then(|e| e.to_str())
                    && extensions
                        .iter()
                        .any(|&target| ext.eq_ignore_ascii_case(target))
                {
                    out.insert(path);
                }
            } else if path.is_dir() {
                let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if !name.starts_with('.')
                    && !matches!(
                        name,
                        "target" | "node_modules" | "tests" | "test" | "sub" | "submissions"
                    )
                {
                    scan_directory_recursive(&path, extensions, max_depth - 1, out);
                }
            }
        }
    }
}

fn scan_includes_transitive(
    initial_bytes: &[u8],
    source_dir: Option<&Path>,
    include_dirs: &[PathBuf],
    out: &mut std::collections::BTreeSet<PathBuf>,
    saw_macro_include: &mut bool,
    max_depth: usize,
) {
    if max_depth == 0 {
        return;
    }
    let text = String::from_utf8_lossy(initial_bytes);
    for line in text.lines() {
        let trimmed = line.trim();
        let Some(after_hash) = trimmed.strip_prefix('#') else {
            continue;
        };
        let after_hash = after_hash.trim_start();
        let Some(rest) = after_hash.strip_prefix("include") else {
            continue;
        };
        // Ensure `#include_next` or identifiers starting with `include` aren't misread unless valid
        let rest = if let Some(r) = rest.strip_prefix("_next") {
            r.trim_start()
        } else if rest.is_empty()
            || rest.starts_with(char::is_whitespace)
            || rest.starts_with('"')
            || rest.starts_with('<')
        {
            rest.trim_start()
        } else {
            continue;
        };

        let target = if rest.starts_with('"') && rest[1..].contains('"') {
            rest.split('"').nth(1)
        } else if rest.starts_with('<') && rest.contains('>') {
            rest.split(['<', '>']).nth(1)
        } else {
            if !rest.is_empty() && !rest.starts_with("//") && !rest.starts_with("/*") {
                *saw_macro_include = true;
            }
            None
        };

        if let Some(inc_rel) = target {
            let rel_path = Path::new(inc_rel);
            let mut resolved = None;
            if let Some(sd) = source_dir {
                let p = sd.join(rel_path);
                if p.is_file() {
                    resolved = Some(p);
                }
            }
            if resolved.is_none() {
                for inc_dir in include_dirs {
                    let p = inc_dir.join(rel_path);
                    if p.is_file() {
                        resolved = Some(p);
                        break;
                    }
                }
            }

            if let Some(p) = resolved
                && out.insert(p.clone())
                && let Ok(content) = fs::read(&p)
            {
                scan_includes_transitive(
                    &content,
                    p.parent(),
                    include_dirs,
                    out,
                    saw_macro_include,
                    max_depth - 1,
                );
            }
        }
    }
}

fn file_stamp(path: &Path) -> Option<(std::time::SystemTime, u64)> {
    let meta = fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    Some((mtime, meta.len()))
}

fn collect_dependency_stamps(
    source_path: &Path,
    extra_sources: &[PathBuf],
    dep_files: &std::collections::BTreeSet<PathBuf>,
    problem_dir: Option<&Path>,
) -> Option<Vec<(PathBuf, std::time::SystemTime, u64)>> {
    let mut stamps = Vec::with_capacity(2 + extra_sources.len() + dep_files.len());
    let (src_mtime, src_len) = file_stamp(source_path)?;
    stamps.push((source_path.to_path_buf(), src_mtime, src_len));
    for extra in extra_sources {
        let (m, l) = file_stamp(extra)?;
        stamps.push((extra.clone(), m, l));
    }
    for dep in dep_files {
        let (m, l) = file_stamp(dep)?;
        stamps.push((dep.clone(), m, l));
    }
    if let Some(pdir) = problem_dir {
        for cfg_name in ["problem.toml", "languages.toml"] {
            let cfg_path = pdir.join(cfg_name);
            if let Some((m, l)) = file_stamp(&cfg_path) {
                stamps.push((cfg_path, m, l));
            }
        }
    }
    Some(stamps)
}

fn stamp_is_racy_safe(exp_m: std::time::SystemTime, recorded_at: std::time::SystemTime) -> bool {
    let Ok(since_epoch) = exp_m.duration_since(std::time::UNIX_EPOCH) else {
        return false;
    };
    let margin = if since_epoch.subsec_nanos() == 0 {
        Duration::from_secs(2)
    } else {
        Duration::from_millis(50)
    };
    recorded_at.duration_since(exp_m).is_ok_and(|d| d >= margin)
}

fn stamps_still_valid(
    stamps: &[(PathBuf, std::time::SystemTime, u64)],
    recorded_at: std::time::SystemTime,
) -> bool {
    stamps.iter().all(|(p, exp_m, exp_l)| {
        stamp_is_racy_safe(*exp_m, recorded_at)
            && file_stamp(p).is_some_and(|(m, l)| m == *exp_m && l == *exp_l)
    })
}

fn artifact_still_exists(res: &CompileResult) -> bool {
    if res.is_interpreted
        && res
            .binary_path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("py"))
    {
        return res.binary_path.is_file();
    }
    fs::metadata(&res.binary_path)
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false)
}

fn compiler_version_disk_key(bin: &Path) -> Option<String> {
    let meta = fs::metadata(bin).ok()?;
    let mtime_ns = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(format!("{}|{}|{}", bin.display(), mtime_ns, meta.len()))
}

fn load_persisted_compiler_version(cache_root: &Path, key: &str) -> Option<String> {
    let path = cache_root.join("compiler_versions.tsv");
    let raw = fs::read_to_string(path).ok()?;
    for line in raw.lines() {
        if let Some((k, v)) = line.split_once('\t')
            && k == key
        {
            return Some(v.to_string());
        }
    }
    None
}

fn save_persisted_compiler_version(cache_root: &Path, key: String, ver: &str) {
    if key.contains(['\t', '\r', '\n']) || ver.contains(['\r', '\n']) {
        return;
    }
    if fs::create_dir_all(cache_root).is_err() {
        return;
    }
    let path = cache_root.join("compiler_versions.tsv");
    let mut map: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    if let Ok(raw) = fs::read_to_string(&path) {
        for line in raw.lines() {
            if let Some((k, v)) = line.split_once('\t') {
                map.insert(k.to_string(), v.to_string());
            }
        }
    }
    if map.get(&key).is_some_and(|v| v == ver) {
        return;
    }
    map.insert(key, ver.to_string());
    let mut out = String::new();
    for (k, v) in &map {
        out.push_str(k);
        out.push('\t');
        out.push_str(v);
        out.push('\n');
    }
    let tmp = cache_root.join(format!("compiler_versions.tmp.{}", std::process::id()));
    if fs::write(&tmp, out.as_bytes()).is_ok() {
        let _ = fs::rename(&tmp, &path);
        let _ = fs::remove_file(&tmp);
    }
}

fn write_atomic_cache_file(dir: &Path, name: &str, content: &[u8], nonce: u64) {
    let pid = std::process::id();
    let tmp = dir.join(format!("{name}.tmp.{pid}.{nonce}"));
    let dst = dir.join(name);
    if fs::write(&tmp, content).is_ok() {
        if fs::rename(&tmp, &dst).is_err() {
            let _ = fs::copy(&tmp, &dst);
        }
        let _ = fs::remove_file(&tmp);
    }
}

impl CompileSession {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn compile(
        &self,
        source_path: &Path,
        no_cache: bool,
    ) -> Result<CompileResult, CompileError> {
        self.compile_with_options(source_path, &[], &[], no_cache)
    }

    /// Compile a problem solution (or std), automatically attaching grader source files
    /// and header include directories when `cfg.problem_type == ProblemType::Grader`.
    pub fn compile_problem_solution(
        &self,
        problem_dir: &Path,
        cfg: &uj_core::config::ProblemConfig,
        source_path: &Path,
        no_cache: bool,
    ) -> Result<CompileResult, CompileError> {
        if cfg.problem_type == uj_core::config::ProblemType::OutputOnly {
            return Ok(CompileResult {
                binary_path: source_path.to_path_buf(),
                is_interpreted: false,
                interpreter_cmd: None,
                compiler_output: String::new(),
                duration_ms: 0,
                cached: false,
            });
        }
        let mut extra_sources = Vec::new();
        let mut include_dirs = vec![problem_dir.to_path_buf()];
        if cfg.problem_type == uj_core::config::ProblemType::Grader
            && let Some(ref grader) = cfg.grader
        {
            for gf in &grader.files {
                let p = if gf.is_absolute() {
                    gf.clone()
                } else {
                    problem_dir.join(gf)
                };
                extra_sources.push(p);
            }
            for gh in &grader.headers {
                let p = if gh.is_absolute() {
                    gh.clone()
                } else {
                    problem_dir.join(gh)
                };
                if let Some(parent) = p.parent() {
                    let parent_buf = parent.to_path_buf();
                    if !include_dirs.contains(&parent_buf) {
                        include_dirs.push(parent_buf);
                    }
                }
            }
        }
        self.compile_with_options(source_path, &extra_sources, &include_dirs, no_cache)
    }

    fn resolve_languages_config(
        &self,
        problem_dir: Option<&Path>,
    ) -> uj_core::config::LanguagesConfig {
        let key = problem_dir.map(|p| p.to_path_buf());
        {
            let cache = self.inner.languages_configs.lock();
            if let Some(cfg) = cache.get(&key) {
                return cfg.clone();
            }
        }

        let mut lang_configs =
            uj_core::config::LanguagesConfig::from_toml_str(DEFAULT_LANGUAGES_TOML, None)
                .unwrap_or_default();

        if let Ok(home) = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")) {
            let global_cfg_path = Path::new(&home)
                .join(".config")
                .join("uj")
                .join("languages.toml");
            if global_cfg_path.is_file()
                && let Ok(content) = fs::read_to_string(&global_cfg_path)
                && let Ok(c) = uj_core::config::LanguagesConfig::from_toml_str(
                    &content,
                    Some(&global_cfg_path),
                )
            {
                lang_configs.merge_overlay(c);
            }
        }

        if let Some(prob_dir) = problem_dir {
            let prob_cfg_path = prob_dir.join("languages.toml");
            if prob_cfg_path.is_file()
                && let Ok(content) = fs::read_to_string(&prob_cfg_path)
                && let Ok(c) =
                    uj_core::config::LanguagesConfig::from_toml_str(&content, Some(&prob_cfg_path))
            {
                lang_configs.merge_overlay(c);
            }
        }

        let mut cache = self.inner.languages_configs.lock();
        cache.insert(key, lang_configs.clone());
        lang_configs
    }

    fn resolve_compiler_bin(&self, cmd_name: &str) -> Result<PathBuf, CompileError> {
        {
            let cache = self.inner.compiler_bins.lock();
            if let Some(res) = cache.get(cmd_name) {
                return res.clone().map_err(CompileError::CompilerNotFound);
            }
        }
        // Probe PATH outside the session mutex so parallel compiles do not serialize on `which`.
        let probed = which::which(cmd_name).map_err(|_| cmd_name.to_string());
        let mut cache = self.inner.compiler_bins.lock();
        let res = cache
            .entry(cmd_name.to_string())
            .or_insert_with(|| probed)
            .clone();
        res.map_err(CompileError::CompilerNotFound)
    }

    fn resolve_compiler_version(
        &self,
        bin: &Path,
        cache_root: Option<&Path>,
        no_cache: bool,
    ) -> String {
        {
            let cache = self.inner.compiler_versions.lock();
            if let Some(ver) = cache.get(bin) {
                return ver.clone();
            }
        }

        let disk_key = if !no_cache {
            compiler_version_disk_key(bin)
        } else {
            None
        };
        if let (Some(root), Some(key)) = (cache_root, disk_key.as_deref())
            && let Some(ver) = load_persisted_compiler_version(root, key)
        {
            let mut cache = self.inner.compiler_versions.lock();
            cache.insert(bin.to_path_buf(), ver.clone());
            return ver;
        }

        // Spawn `<compiler> --version` outside the session mutex so parallel compiles don't block.
        let mut cmd = Command::new(bin);
        cmd.arg("--version");
        let version_line = match run_command_with_timeout(cmd, Duration::from_secs(5)) {
            Ok((_, stdout, _)) => {
                let text = String::from_utf8_lossy(&stdout);
                text.lines().next().unwrap_or("unknown").to_string()
            }
            Err(_) => "unknown".to_string(),
        };

        if let (Some(root), Some(key)) = (cache_root, disk_key)
            && version_line != "unknown"
        {
            save_persisted_compiler_version(root, key, &version_line);
        }

        let mut cache = self.inner.compiler_versions.lock();
        cache
            .entry(bin.to_path_buf())
            .or_insert(version_line)
            .clone()
    }

    fn resolve_problem_limits(&self, problem_dir: Option<&Path>) -> (u64, u64, u64) {
        let Some(pdir) = problem_dir else {
            return (256, 256, 268435456);
        };
        {
            let cache = self.inner.problem_limits.lock();
            if let Some(&limits) = cache.get(pdir) {
                return limits;
            }
        }

        let mut mem_mb: u64 = 256;
        let mut stack_mb: u64 = 256;
        let mut stack_bytes: u64 = 268435456;

        let ptoml = pdir.join("problem.toml");
        if ptoml.is_file()
            && let Ok(content) = fs::read_to_string(&ptoml)
            && let Ok(pcfg) = uj_core::config::ProblemConfig::from_toml_str(&content, Some(&ptoml))
        {
            if let Ok(b) = uj_core::config::parse_bytes(&pcfg.limits.memory) {
                mem_mb = b / (1024 * 1024);
            }
            if let Some(ref stk) = pcfg.limits.stack {
                if stk.eq_ignore_ascii_case("same") || stk.eq_ignore_ascii_case("unlimited") {
                    stack_mb = mem_mb;
                    stack_bytes = stack_mb * 1024 * 1024;
                } else if let Ok(b) = uj_core::config::parse_bytes(stk) {
                    stack_bytes = b;
                    stack_mb = b / (1024 * 1024);
                }
            } else {
                stack_mb = mem_mb;
                stack_bytes = stack_mb * 1024 * 1024;
            }
        }

        let res = (mem_mb, stack_mb, stack_bytes);
        let mut cache = self.inner.problem_limits.lock();
        cache.insert(pdir.to_path_buf(), res);
        res
    }

    fn record_session_outcome(
        &self,
        cache_hash: String,
        precheck_key: PrecheckKey,
        stamps: Option<Vec<(PathBuf, std::time::SystemTime, u64)>>,
        outcome: CachedOutcome,
    ) {
        self.inner
            .fast_results
            .lock()
            .insert(cache_hash, outcome.clone());
        if let Some(stamps) = stamps {
            self.inner.mtime_precheck.lock().insert(
                precheck_key,
                PrecheckEntry {
                    stamps,
                    recorded_at: std::time::SystemTime::now(),
                    outcome,
                },
            );
        }
    }

    /// Compile source file with extra source files and include directories using this session's cache.
    pub fn compile_with_options(
        &self,
        source_path: &Path,
        extra_sources: &[PathBuf],
        include_dirs: &[PathBuf],
        no_cache: bool,
    ) -> Result<CompileResult, CompileError> {
        let start = Instant::now();
        static COMPILE_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

        let precheck_key = (
            source_path.to_path_buf(),
            extra_sources.to_vec(),
            include_dirs.to_vec(),
        );

        // Cheap mtime/size precheck across source + extra_sources + headers + config files
        if !no_cache {
            let cached_entry = self.inner.mtime_precheck.lock().get(&precheck_key).cloned();
            if let Some(entry) = cached_entry
                && stamps_still_valid(&entry.stamps, entry.recorded_at)
            {
                match entry.outcome {
                    CachedOutcome::Success(mut res) if artifact_still_exists(&res) => {
                        res.cached = true;
                        res.duration_ms = start.elapsed().as_millis() as u64;
                        return Ok(res);
                    }
                    CachedOutcome::Failed(err) => {
                        return Err(CompileError::Failed(err));
                    }
                    _ => {}
                }
            }
        }

        let source_bytes = fs::read(source_path)?;

        let problem_dir = source_path
            .ancestors()
            .chain(include_dirs.iter().map(|p| p.as_path()))
            .find(|p| p.join("problem.toml").exists() || p.join(".uj").exists());

        let cache_root_dir = problem_dir
            .map(|p| p.join(".uj").join("cache"))
            .unwrap_or_else(|| PathBuf::from(".uj/cache"));
        let base_cache_dir = cache_root_dir.join("bin");

        // 1. Load and merge languages.toml using session cache
        let lang_configs = self.resolve_languages_config(problem_dir);

        let ext = source_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default();
        let (lang_id, lang_cfg) = lang_configs
            .find_by_extension(ext)
            .ok_or_else(|| CompileError::UnsupportedExtension(ext.to_string()))?;

        // 2. Handle interpreted languages (e.g. Python) with session + disk syntax-check caching
        if ext.eq_ignore_ascii_case("py") {
            let python_bin = self
                .resolve_compiler_bin("python3")
                .or_else(|_| self.resolve_compiler_bin("python"))
                .map_err(|_| CompileError::CompilerNotFound("python3/python".to_string()))?;

            let mut py_hasher = blake3::Hasher::new();
            py_hasher.update(b"python-py-compile-v1:");
            py_hasher.update(python_bin.to_string_lossy().as_bytes());
            py_hasher.update(b":");
            py_hasher.update(&source_bytes);
            let py_hash = py_hasher.finalize().to_hex().to_string();
            let empty_deps = std::collections::BTreeSet::new();
            let stamps =
                collect_dependency_stamps(source_path, extra_sources, &empty_deps, problem_dir);

            let build_py_res = |cached: bool, dur: u64| CompileResult {
                binary_path: source_path.to_path_buf(),
                is_interpreted: true,
                interpreter_cmd: Some(vec![
                    python_bin.to_string_lossy().to_string(),
                    source_path.to_string_lossy().to_string(),
                ]),
                compiler_output: String::new(),
                duration_ms: dur,
                cached,
            };

            if !no_cache {
                if let Some(outcome) = self.inner.fast_results.lock().get(&py_hash).cloned() {
                    match outcome {
                        CachedOutcome::Success(_) => {
                            let res = build_py_res(true, start.elapsed().as_millis() as u64);
                            self.record_session_outcome(
                                py_hash,
                                precheck_key,
                                stamps,
                                CachedOutcome::Success(res.clone()),
                            );
                            return Ok(res);
                        }
                        CachedOutcome::Failed(err) => {
                            return Err(CompileError::Failed(err));
                        }
                    }
                }
                let py_cache_dir = base_cache_dir.join(&py_hash);
                if py_cache_dir.join("py.ok").is_file() {
                    let res = build_py_res(true, start.elapsed().as_millis() as u64);
                    self.record_session_outcome(
                        py_hash,
                        precheck_key,
                        stamps,
                        CachedOutcome::Success(res.clone()),
                    );
                    return Ok(res);
                }
                if let Ok(err) = fs::read_to_string(py_cache_dir.join("ce.txt")) {
                    self.record_session_outcome(
                        py_hash,
                        precheck_key,
                        stamps,
                        CachedOutcome::Failed(err.clone()),
                    );
                    return Err(CompileError::Failed(err));
                }
            }

            let mut py_cmd = Command::new(&python_bin);
            py_cmd.arg("-m").arg("py_compile").arg(source_path);

            let (status, _stdout, stderr) =
                run_command_with_timeout(py_cmd, Duration::from_secs(10))?;

            let nonce = COMPILE_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let py_cache_dir = base_cache_dir.join(&py_hash);
            if !status.success() {
                let err_msg = truncate_10k(&String::from_utf8_lossy(&stderr));
                if !no_cache && fs::create_dir_all(&py_cache_dir).is_ok() {
                    write_atomic_cache_file(&py_cache_dir, "ce.txt", err_msg.as_bytes(), nonce);
                    self.record_session_outcome(
                        py_hash,
                        precheck_key,
                        stamps,
                        CachedOutcome::Failed(err_msg.clone()),
                    );
                }
                return Err(CompileError::Failed(err_msg));
            }

            let res = build_py_res(false, start.elapsed().as_millis() as u64);
            if !no_cache && fs::create_dir_all(&py_cache_dir).is_ok() {
                write_atomic_cache_file(&py_cache_dir, "py.ok", b"ok", nonce);
                self.record_session_outcome(
                    py_hash,
                    precheck_key,
                    stamps,
                    CachedOutcome::Success(res.clone()),
                );
            }
            return Ok(res);
        }

        // 3. Resolve compile command template from lang_cfg
        let compile_template = lang_cfg.compile.as_ref().ok_or_else(|| {
            CompileError::Failed(format!(
                "Language '{lang_id}' has no compile command defined"
            ))
        })?;

        let compiler_cmd_name = &compile_template[0];
        let compiler_bin = self.resolve_compiler_bin(compiler_cmd_name)?;

        // Query compiler version using session + persisted cache
        let version_line = self.resolve_compiler_version(
            &compiler_bin,
            if problem_dir.is_some() {
                Some(cache_root_dir.as_path())
            } else {
                None
            },
            no_cache,
        );

        // Dynamically resolve memory_mb, stack_mb, and stack_bytes from problem.toml using session cache
        let (mem_mb, stack_mb, stack_bytes) = self.resolve_problem_limits(problem_dir);

        let uses_placeholder = |needle: &str| -> bool {
            compile_template.iter().any(|s| s.contains(needle))
                || lang_cfg.run.iter().any(|s| s.contains(needle))
                || (cfg!(windows)
                    && lang_cfg
                        .compile_windows_extra
                        .as_ref()
                        .is_some_and(|v| v.iter().any(|s| s.contains(needle))))
        };

        // 4. Compute Blake3 cache key (including source, flags, version, and actual dependencies)
        let mut hasher = blake3::Hasher::new();
        hasher.update(lang_id.as_bytes());
        hasher.update(version_line.as_bytes());
        if uses_placeholder("{memory_mb}") {
            hasher.update(b"mem:");
            hasher.update(mem_mb.to_string().as_bytes());
        }
        if uses_placeholder("{stack_mb}") {
            hasher.update(b"stk_mb:");
            hasher.update(stack_mb.to_string().as_bytes());
        }
        if uses_placeholder("{stack_bytes}") {
            hasher.update(b"stk_b:");
            hasher.update(stack_bytes.to_string().as_bytes());
        }
        for t in compile_template {
            hasher.update(t.as_bytes());
        }
        if cfg!(windows)
            && let Some(ref win_extra) = lang_cfg.compile_windows_extra
        {
            for arg in win_extra {
                hasher.update(arg.as_bytes());
            }
        }
        hasher.update(&source_bytes);
        let mut extra_contents: Vec<(&Path, Vec<u8>)> = Vec::with_capacity(extra_sources.len());
        for extra in extra_sources {
            if let Ok(b) = fs::read(extra) {
                hasher.update(extra.to_string_lossy().as_bytes());
                hasher.update(&b);
                extra_contents.push((extra.as_path(), b));
            }
        }

        // Include dependencies: transitive #include scan for C/C++ (falling back to directory scan
        // only if a computed macro `#include MACRO` was seen), module tree scan for Rust/Go.
        let mut dep_files = std::collections::BTreeSet::new();
        let header_exts = &["h", "hpp", "hh", "hxx", "h++", "inc", "cuh", "inl", "tpp"];
        let is_cpp_or_c = lang_id.starts_with("cpp") || lang_id.starts_with("c");
        let is_rust = lang_id.starts_with("rust");
        let is_go = lang_id.starts_with("go");

        if is_cpp_or_c {
            let mut saw_macro_include = false;
            scan_includes_transitive(
                &source_bytes,
                source_path.parent(),
                include_dirs,
                &mut dep_files,
                &mut saw_macro_include,
                12,
            );
            for (extra_path, extra_bytes) in &extra_contents {
                scan_includes_transitive(
                    extra_bytes,
                    extra_path.parent(),
                    include_dirs,
                    &mut dep_files,
                    &mut saw_macro_include,
                    12,
                );
            }
            if saw_macro_include {
                if let Some(p) = source_path.parent() {
                    scan_directory_recursive(p, header_exts, 2, &mut dep_files);
                }
                for inc in include_dirs {
                    scan_directory_recursive(inc, header_exts, 2, &mut dep_files);
                }
            }
        } else if is_rust {
            if let Some(p) = source_path.parent() {
                scan_directory_recursive(p, &["rs"], 5, &mut dep_files);
                if p.file_name().is_some_and(|n| n == "src")
                    && let Some(grandparent) = p.parent()
                {
                    scan_directory_recursive(grandparent, &["rs"], 5, &mut dep_files);
                }
            }
        } else if is_go && let Some(p) = source_path.parent() {
            scan_directory_recursive(p, &["go"], 5, &mut dep_files);
        }

        for dep in &dep_files {
            if let Ok(bytes) = fs::read(dep) {
                hasher.update(dep.to_string_lossy().as_bytes());
                hasher.update(&bytes);
            }
        }

        let stamps = collect_dependency_stamps(source_path, extra_sources, &dep_files, problem_dir);
        let cache_hash = hasher.finalize().to_hex().to_string();

        let is_java = lang_id == "java";
        let java_class_name = source_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Main");

        if !no_cache {
            let fast_hit = self.inner.fast_results.lock().get(&cache_hash).cloned();
            match fast_hit {
                Some(CachedOutcome::Success(mut res)) if artifact_still_exists(&res) => {
                    res.cached = true;
                    res.duration_ms = start.elapsed().as_millis() as u64;
                    if is_java {
                        let cache_dir = base_cache_dir.join(&cache_hash);
                        res.interpreter_cmd = Some(
                            lang_cfg
                                .run
                                .iter()
                                .map(|part| {
                                    part.replace("{dir}", &cache_dir.to_string_lossy())
                                        .replace("{stack_mb}", &stack_mb.to_string())
                                        .replace("{memory_mb}", &mem_mb.to_string())
                                        .replace("{stack_bytes}", &stack_bytes.to_string())
                                        .replace("Main", java_class_name)
                                })
                                .collect(),
                        );
                    }
                    if let Some(stamps) = stamps {
                        self.inner.mtime_precheck.lock().insert(
                            precheck_key,
                            PrecheckEntry {
                                stamps,
                                recorded_at: std::time::SystemTime::now(),
                                outcome: CachedOutcome::Success(res.clone()),
                            },
                        );
                    }
                    return Ok(res);
                }
                Some(CachedOutcome::Failed(err)) => {
                    if let Some(stamps) = stamps {
                        self.inner.mtime_precheck.lock().insert(
                            precheck_key,
                            PrecheckEntry {
                                stamps,
                                recorded_at: std::time::SystemTime::now(),
                                outcome: CachedOutcome::Failed(err.clone()),
                            },
                        );
                    }
                    return Err(CompileError::Failed(err));
                }
                _ => {}
            }
        }

        let hash_lock = if !no_cache {
            let mut inflight = self.inner.inflight.lock();
            inflight
                .entry(cache_hash.clone())
                .or_insert_with(|| Arc::new(parking_lot::Mutex::new(())))
                .clone()
        } else {
            Arc::new(parking_lot::Mutex::new(()))
        };
        let _compile_guard = hash_lock.lock();

        if !no_cache {
            let fast_hit = self.inner.fast_results.lock().get(&cache_hash).cloned();
            match fast_hit {
                Some(CachedOutcome::Success(mut res)) if artifact_still_exists(&res) => {
                    res.cached = true;
                    res.duration_ms = start.elapsed().as_millis() as u64;
                    return Ok(res);
                }
                Some(CachedOutcome::Failed(err)) => {
                    return Err(CompileError::Failed(err));
                }
                _ => {}
            }
        }
        let cache_dir = base_cache_dir.join(&cache_hash);
        let exe_ext = if cfg!(windows) { ".exe" } else { "" };
        let cached_bin = cache_dir.join(format!("prog{exe_ext}"));
        let java_class_file = cache_dir.join(format!("{java_class_name}.class"));
        let ce_file = cache_dir.join("ce.txt");

        let target_artifact = if is_java {
            &java_class_file
        } else {
            &cached_bin
        };

        if !no_cache {
            if target_artifact.exists()
                && fs::metadata(target_artifact)
                    .map(|m| m.len() > 0)
                    .unwrap_or(false)
            {
                let interpreter_cmd = if is_java {
                    let mut cmd = Vec::new();
                    for part in &lang_cfg.run {
                        let replaced = part
                            .replace("{dir}", &cache_dir.to_string_lossy())
                            .replace("{stack_mb}", &stack_mb.to_string())
                            .replace("{memory_mb}", &mem_mb.to_string())
                            .replace("{stack_bytes}", &stack_bytes.to_string())
                            .replace("Main", java_class_name);
                        cmd.push(replaced);
                    }
                    Some(cmd)
                } else {
                    None
                };
                let res = CompileResult {
                    binary_path: target_artifact.clone(),
                    is_interpreted: is_java,
                    interpreter_cmd,
                    compiler_output: String::new(),
                    duration_ms: start.elapsed().as_millis() as u64,
                    cached: true,
                };
                self.record_session_outcome(
                    cache_hash,
                    precheck_key,
                    stamps,
                    CachedOutcome::Success(res.clone()),
                );
                return Ok(res);
            }
            if let Ok(cached_ce) = fs::read_to_string(&ce_file) {
                self.record_session_outcome(
                    cache_hash,
                    precheck_key,
                    stamps,
                    CachedOutcome::Failed(cached_ce.clone()),
                );
                return Err(CompileError::Failed(cached_ce));
            }
        }

        fs::create_dir_all(&cache_dir)?;

        let nonce = COMPILE_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let pid = std::process::id();
        let tmp_bin = cache_dir.join(format!("prog.tmp.{pid}.{nonce}{exe_ext}"));
        let tmp_java_dir = cache_dir.join(format!("javac_tmp_{pid}_{nonce}"));
        if is_java {
            fs::create_dir_all(&tmp_java_dir)?;
        }

        // 5. Construct compiler command arguments (writing to temporary path first)
        let mut final_args = Vec::new();
        for arg in &compile_template[1..] {
            match arg.as_str() {
                "{exe}" => final_args.push(tmp_bin.to_string_lossy().to_string()),
                "{src}" => final_args.push(source_path.to_string_lossy().to_string()),
                "{dir}" => {
                    let out_dir = if is_java { &tmp_java_dir } else { &cache_dir };
                    final_args.push(out_dir.to_string_lossy().to_string());
                }
                "{extra_srcs}" => {
                    for extra in extra_sources {
                        final_args.push(extra.to_string_lossy().to_string());
                    }
                }
                "{stack_bytes}" => final_args.push(stack_bytes.to_string()),
                "{stack_mb}" => final_args.push(stack_mb.to_string()),
                "{memory_mb}" => final_args.push(mem_mb.to_string()),
                other => final_args.push(other.to_string()),
            }
        }

        // Add include dirs
        for inc in include_dirs {
            final_args.push("-I".to_string());
            final_args.push(inc.to_string_lossy().to_string());
        }

        #[cfg(windows)]
        {
            if let Some(ref win_extra) = lang_cfg.compile_windows_extra {
                for extra in win_extra {
                    let replaced = extra
                        .replace("{stack_bytes}", &stack_bytes.to_string())
                        .replace("{stack_mb}", &stack_mb.to_string());
                    final_args.push(replaced);
                }
            }
        }

        let cleanup_tmp = || {
            let _ = fs::remove_file(&tmp_bin);
            if is_java {
                let _ = fs::remove_dir_all(&tmp_java_dir);
            }
        };

        // 6. Spawn compiler process with timeout and concurrent reader protection
        let mut cmd = Command::new(&compiler_bin);
        cmd.args(&final_args);
        let (status, stdout_bytes, stderr_bytes) =
            match run_command_with_timeout(cmd, Duration::from_secs(30)) {
                Ok(res) => res,
                Err(e) => {
                    cleanup_tmp();
                    return Err(e);
                }
            };
        let duration_ms = start.elapsed().as_millis() as u64;

        let combined_output = format!(
            "{}{}",
            String::from_utf8_lossy(&stdout_bytes),
            String::from_utf8_lossy(&stderr_bytes)
        );

        if !status.success() {
            cleanup_tmp();
            let truncated = truncate_10k(&combined_output);
            if !no_cache {
                write_atomic_cache_file(&cache_dir, "ce.txt", truncated.as_bytes(), nonce);
                self.record_session_outcome(
                    cache_hash,
                    precheck_key,
                    stamps,
                    CachedOutcome::Failed(truncated.clone()),
                );
            }
            return Err(CompileError::Failed(truncated));
        }

        // Remove any stale CE marker if present
        let _ = fs::remove_file(&ce_file);

        // 7. Verify temporary artifact and atomically promote into cache_dir
        if is_java {
            let tmp_main_class = tmp_java_dir.join(format!("{java_class_name}.class"));
            if !tmp_main_class.exists()
                || fs::metadata(&tmp_main_class)
                    .map(|m| m.len() == 0)
                    .unwrap_or(true)
            {
                cleanup_tmp();
                return Err(CompileError::Failed(
                    "Compiler succeeded but produced no valid class file".to_string(),
                ));
            }
            if let Ok(entries) = fs::read_dir(&tmp_java_dir) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p != tmp_main_class
                        && let Some(fname) = p.file_name()
                    {
                        let dst = cache_dir.join(fname);
                        let _ = fs::rename(&p, &dst).or_else(|_| fs::copy(&p, &dst).map(|_| ()));
                    }
                }
            }
            if let Err(e) = fs::rename(&tmp_main_class, &java_class_file)
                && !java_class_file.exists()
            {
                cleanup_tmp();
                return Err(CompileError::Io(e));
            }
            let _ = fs::remove_dir_all(&tmp_java_dir);
        } else {
            if !tmp_bin.exists() || fs::metadata(&tmp_bin).map(|m| m.len() == 0).unwrap_or(true) {
                cleanup_tmp();
                return Err(CompileError::Failed(
                    "Compiler succeeded but produced no valid binary file".to_string(),
                ));
            }
            if let Err(e) = fs::rename(&tmp_bin, &cached_bin) {
                let _ = fs::remove_file(&tmp_bin);
                if !cached_bin.exists()
                    || fs::metadata(&cached_bin)
                        .map(|m| m.len() == 0)
                        .unwrap_or(true)
                {
                    return Err(CompileError::Io(e));
                }
            }
        }

        let interpreter_cmd = if is_java {
            let mut cmd = Vec::new();
            for part in &lang_cfg.run {
                let replaced = part
                    .replace("{dir}", &cache_dir.to_string_lossy())
                    .replace("{stack_mb}", &stack_mb.to_string())
                    .replace("{memory_mb}", &mem_mb.to_string())
                    .replace("{stack_bytes}", &stack_bytes.to_string())
                    .replace("Main", java_class_name);
                cmd.push(replaced);
            }
            Some(cmd)
        } else {
            None
        };

        let res = CompileResult {
            binary_path: target_artifact.clone(),
            is_interpreted: is_java,
            interpreter_cmd,
            compiler_output: truncate_10k(&combined_output),
            duration_ms,
            cached: false,
        };

        if !no_cache {
            self.record_session_outcome(
                cache_hash,
                precheck_key,
                stamps,
                CachedOutcome::Success(res.clone()),
            );
        }

        Ok(res)
    }
}

pub fn compute_sources_hash(source_path: &Path, extra_sources: &[PathBuf]) -> Option<String> {
    let mut hasher = blake3::Hasher::new();
    let src_bytes = fs::read(source_path).ok()?;
    hasher.update(&src_bytes);
    for extra in extra_sources {
        let b = fs::read(extra).ok()?;
        hasher.update(extra.to_string_lossy().as_bytes());
        hasher.update(&b);
    }
    Some(hasher.finalize().to_hex().to_string())
}

pub struct Compiler;

impl Compiler {
    /// Compile source file, utilizing .uj/cache/bin/<blake3> caching.
    pub fn compile(source_path: &Path, no_cache: bool) -> Result<CompileResult, CompileError> {
        CompileSession::new().compile(source_path, no_cache)
    }

    /// Compile source file with extra source files (e.g. grader.cpp) and include directories.
    pub fn compile_with_options(
        source_path: &Path,
        extra_sources: &[PathBuf],
        include_dirs: &[PathBuf],
        no_cache: bool,
    ) -> Result<CompileResult, CompileError> {
        CompileSession::new().compile_with_options(
            source_path,
            extra_sources,
            include_dirs,
            no_cache,
        )
    }
}

fn truncate_10k(s: &str) -> String {
    const LIMIT: usize = 10 * 1024;
    if s.len() > LIMIT {
        let mut end = LIMIT;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\n... [Output truncated to 10 KiB]", &s[..end])
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_language_detection() {
        assert_eq!(
            Language::from_path(Path::new("sol.cpp")).unwrap(),
            Language::Cpp
        );
        assert_eq!(
            Language::from_path(Path::new("sol.c")).unwrap(),
            Language::C
        );
        assert_eq!(
            Language::from_path(Path::new("sol.rs")).unwrap(),
            Language::Rust
        );
        assert_eq!(
            Language::from_path(Path::new("sol.py")).unwrap(),
            Language::Python
        );
        assert_eq!(
            Language::from_path(Path::new("sol.java")).unwrap(),
            Language::Java
        );
        assert!(Language::from_path(Path::new("sol.xyz")).is_err());
    }

    #[test]
    fn test_truncate_10k() {
        let short = "abc";
        assert_eq!(truncate_10k(short), "abc");

        let long = "a".repeat(20000);
        let truncated = truncate_10k(&long);
        assert!(truncated.len() <= 10240 + 100);
        assert!(truncated.contains("Output truncated to 10 KiB"));
    }

    #[test]
    fn test_truncate_10k_multibyte() {
        let s = "你好世界！".repeat(2000);
        let truncated = truncate_10k(&s);
        assert!(truncated.contains("Output truncated to 10 KiB"));
    }

    #[test]
    fn test_compile_session_fast_cache() {
        let tmp_dir = std::env::temp_dir().join(format!("uj_test_cache_{}", std::process::id()));
        let _ = fs::create_dir_all(&tmp_dir);
        let src = tmp_dir.join("test_sol.cpp");
        fs::write(&src, b"int main() { return 0; }").unwrap();

        let session = CompileSession::new();
        // First compilation creates/resolves binary
        if let Ok(res1) = session.compile(&src, false) {
            assert!(res1.binary_path.exists());

            // Second compilation within same session should hit fast memory cache
            let res2 = session.compile(&src, false).unwrap();
            assert!(res2.cached);
            assert_eq!(res2.binary_path, res1.binary_path);
        }

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_compute_sources_hash_with_extra_sources() {
        let tmp_dir = std::env::temp_dir().join(format!("uj_test_stamp_{}", std::process::id()));
        let _ = fs::create_dir_all(&tmp_dir);
        let src = tmp_dir.join("sol.cpp");
        let extra = tmp_dir.join("grader.cpp");
        fs::write(&src, b"sol").unwrap();
        fs::write(&extra, b"grader").unwrap();

        let hash1 = compute_sources_hash(&src, std::slice::from_ref(&extra)).unwrap();

        // Modifying extra source updates the content hash
        fs::write(&extra, b"grader updated content").unwrap();
        let hash2 = compute_sources_hash(&src, &[extra]).unwrap();
        assert_ne!(hash1, hash2);

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_compile_same_source_concurrently() {
        let tmp_dir =
            std::env::temp_dir().join(format!("uj_test_concurrent_comp_{}", std::process::id()));
        let _ = fs::create_dir_all(&tmp_dir);
        let src = tmp_dir.join("aplusb.cpp");
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let code = format!(
            "#include <iostream>\n// nonce: {nonce}\nint main() {{ int a, b; if (std::cin >> a >> b) std::cout << a + b << std::endl; return 0; }}\n"
        );
        fs::write(&src, code.as_bytes()).unwrap();

        let session = Arc::new(CompileSession::new());
        let mut handles = Vec::new();

        for _ in 0..8 {
            let session_clone = Arc::clone(&session);
            let src_clone = src.clone();
            handles.push(std::thread::spawn(move || {
                session_clone.compile(&src_clone, false)
            }));
        }

        let mut results = Vec::new();
        for h in handles {
            let res = h.join().unwrap();
            results.push(res);
        }

        if results[0].is_ok() {
            let mut uncached_count = 0;
            for r in &results {
                let res = r.as_ref().unwrap();
                assert!(res.binary_path.exists());
                if !res.cached {
                    uncached_count += 1;
                }
            }
            assert_eq!(
                uncached_count, 1,
                "Only 1 thread should compile, others hit cache"
            );
        }

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_scan_directory_recursive_skips_tests_and_submissions() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("include")).unwrap();
        fs::create_dir_all(root.join("tests")).unwrap();
        fs::create_dir_all(root.join("submissions")).unwrap();
        fs::write(root.join("include").join("grader.h"), b"// header").unwrap();
        fs::write(root.join("tests").join("secret.h"), b"// test header").unwrap();
        fs::write(root.join("submissions").join("sub.h"), b"// sub header").unwrap();

        let mut out = std::collections::BTreeSet::new();
        scan_directory_recursive(root, &["h"], 3, &mut out);
        assert!(out.contains(&root.join("include").join("grader.h")));
        assert!(!out.contains(&root.join("tests").join("secret.h")));
        assert!(!out.contains(&root.join("submissions").join("sub.h")));
    }

    #[test]
    fn test_run_command_with_timeout_does_not_hang() {
        #[cfg(windows)]
        let cmd = {
            let mut c = Command::new("cmd");
            c.args(["/C", "ping -n 10 127.0.0.1 >NUL"]);
            c
        };
        #[cfg(unix)]
        let cmd = {
            let mut c = Command::new("sh");
            c.args(["-c", "sleep 10 & sleep 10"]);
            c
        };

        let start = Instant::now();
        let res = run_command_with_timeout(cmd, Duration::from_millis(300));
        assert!(res.is_err());
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "Timeout handler should return promptly without waiting for 10s sleep"
        );
    }

    #[test]
    fn test_compile_session_header_edit_invalidates_precheck() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join(".uj")).unwrap();
        let hdr = root.join("val.h");
        let src = root.join("sol.cpp");
        fs::write(&hdr, b"#define RET_VAL 0\n").unwrap();
        fs::write(
            &src,
            b"#include \"val.h\"\nint main() { return RET_VAL; }\n",
        )
        .unwrap();

        let session = CompileSession::new();
        if let Ok(res1) = session.compile(&src, false) {
            // Edit only the included header (new size so stamp changes immediately)
            fs::write(&hdr, b"#define RET_VAL (0 + 0)\n").unwrap();
            let res2 = session.compile(&src, false).unwrap();
            assert_ne!(
                res1.binary_path, res2.binary_path,
                "Editing an included header must produce a new cache key even in the same session"
            );
        }
    }

    #[test]
    fn test_compile_error_is_cached_on_disk_and_in_session() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join(".uj")).unwrap();
        let src = root.join("bad.cpp");
        fs::write(&src, b"int main() { this_does_not_compile; }\n").unwrap();

        let session1 = CompileSession::new();
        let err1 = match session1.compile(&src, false) {
            Err(CompileError::CompilerNotFound(_)) => return,
            Err(CompileError::Failed(msg)) => msg,
            other => panic!("expected CompileError::Failed, got {other:?}"),
        };
        assert!(!err1.is_empty());

        // In-session negative hit
        let err_in_session = match session1.compile(&src, false) {
            Err(CompileError::Failed(msg)) => msg,
            other => panic!("expected cached in-session CE, got {other:?}"),
        };
        assert_eq!(err1, err_in_session);

        // Fresh-session disk `ce.txt` hit
        let session2 = CompileSession::new();
        let t0 = Instant::now();
        let err_disk = match session2.compile(&src, false) {
            Err(CompileError::Failed(msg)) => msg,
            other => panic!("expected disk-cached CE, got {other:?}"),
        };
        assert_eq!(err1, err_disk);
        assert!(
            t0.elapsed() < Duration::from_millis(500),
            "Disk-cached CE should return without re-invoking compiler"
        );
    }
}
