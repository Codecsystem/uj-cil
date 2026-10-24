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
type FastCompileValue = CompileResult;
type PrecheckKey = (PathBuf, Vec<PathBuf>, Vec<PathBuf>);
type PrecheckValue = (std::time::SystemTime, u64, CompileResult);

#[derive(Default)]
struct CompileSessionInner {
    compiler_bins: parking_lot::Mutex<std::collections::HashMap<String, Result<PathBuf, String>>>,
    compiler_versions: parking_lot::Mutex<std::collections::HashMap<PathBuf, String>>,
    languages_configs: parking_lot::Mutex<
        std::collections::HashMap<Option<PathBuf>, uj_core::config::LanguagesConfig>,
    >,
    problem_limits: parking_lot::Mutex<std::collections::HashMap<PathBuf, (u64, u64, u64)>>,
    fast_results: parking_lot::Mutex<std::collections::HashMap<FastCompileKey, FastCompileValue>>,
    mtime_precheck: parking_lot::Mutex<std::collections::HashMap<PrecheckKey, PrecheckValue>>,
    inflight: parking_lot::Mutex<std::collections::HashMap<String, Arc<parking_lot::Mutex<()>>>>,
}

fn kill_process_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        let pid = child.id() as i32;
        if pid > 0 {
            // Child was spawned with process_group(0), so its pgid equals its pid.
            // Signal -pgid with SIGKILL (9) to terminate all descendants.
            unsafe {
                let _ = kill(-pid, 9);
            }
        }
    }
    #[cfg(windows)]
    {
        let pid_str = child.id().to_string();
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid_str])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
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
    let start = Instant::now();

    let mut stdout_pipe = child.stdout.take().unwrap();
    let mut stderr_pipe = child.stderr.take().unwrap();

    let (stdout_tx, stdout_rx) = crossbeam_channel::bounded(1);
    let (stderr_tx, stderr_rx) = crossbeam_channel::bounded(1);

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

    let mut timed_out = false;
    let mut exit_status = None;

    loop {
        match child.try_wait()? {
            Some(st) => {
                exit_status = Some(st);
                break;
            }
            None => {
                if start.elapsed() > timeout {
                    timed_out = true;
                    kill_process_tree(&mut child);
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

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

    let status = exit_status.unwrap_or_else(|| child.wait().unwrap());
    Ok((status, stdout_bytes, stderr_bytes))
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
    max_depth: usize,
) {
    if max_depth == 0 {
        return;
    }
    let text = String::from_utf8_lossy(initial_bytes);
    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("#include") {
            continue;
        }
        let rest = trimmed.strip_prefix("#include").unwrap().trim();
        let target = if rest.starts_with('"') && rest.contains('"') {
            rest.split('"').nth(1)
        } else if rest.starts_with('<') && rest.contains('>') {
            rest.split(['<', '>']).nth(1)
        } else {
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
                scan_includes_transitive(&content, p.parent(), include_dirs, out, max_depth - 1);
            }
        }
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
        let mut cache = self.inner.compiler_bins.lock();
        if let Some(res) = cache.get(cmd_name) {
            return res.clone().map_err(CompileError::CompilerNotFound);
        }
        let res = which::which(cmd_name).map_err(|_| cmd_name.to_string());
        cache.insert(cmd_name.to_string(), res.clone());
        res.map_err(CompileError::CompilerNotFound)
    }

    fn resolve_compiler_version(&self, bin: &Path) -> String {
        let mut cache = self.inner.compiler_versions.lock();
        if let Some(ver) = cache.get(bin) {
            return ver.clone();
        }
        let mut cmd = Command::new(bin);
        cmd.arg("--version");
        let version_line = match run_command_with_timeout(cmd, Duration::from_secs(5)) {
            Ok((_, stdout, _)) => {
                let text = String::from_utf8_lossy(&stdout);
                text.lines().next().unwrap_or("unknown").to_string()
            }
            Err(_) => "unknown".to_string(),
        };
        cache.insert(bin.to_path_buf(), version_line.clone());
        version_line
    }

    fn resolve_problem_limits(&self, problem_dir: Option<&Path>) -> (u64, u64, u64) {
        let Some(pdir) = problem_dir else {
            return (256, 256, 268435456);
        };
        let mut cache = self.inner.problem_limits.lock();
        if let Some(&limits) = cache.get(pdir) {
            return limits;
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
        cache.insert(pdir.to_path_buf(), res);
        res
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

        // Cheap mtime/size precheck before reading source or scanning dependencies
        if !no_cache {
            let key = (
                source_path.to_path_buf(),
                extra_sources.to_vec(),
                include_dirs.to_vec(),
            );
            if let Ok(meta) = fs::metadata(source_path)
                && let Ok(mtime) = meta.modified()
            {
                let size = meta.len();
                let precheck = self.inner.mtime_precheck.lock();
                if let Some((cached_mtime, cached_size, cached_res)) = precheck.get(&key)
                    && *cached_mtime == mtime
                    && *cached_size == size
                    && cached_res.binary_path.exists()
                    && fs::metadata(&cached_res.binary_path)
                        .map(|m| m.len() > 0)
                        .unwrap_or(false)
                {
                    let mut res = cached_res.clone();
                    res.cached = true;
                    res.duration_ms = start.elapsed().as_millis() as u64;
                    return Ok(res);
                }
            }
        }

        let source_bytes = fs::read(source_path)?;

        let problem_dir = source_path
            .ancestors()
            .chain(include_dirs.iter().map(|p| p.as_path()))
            .find(|p| p.join("problem.toml").exists() || p.join(".uj").exists());

        // 1. Load and merge languages.toml using session cache
        let lang_configs = self.resolve_languages_config(problem_dir);

        let ext = source_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default();
        let (lang_id, lang_cfg) = lang_configs
            .find_by_extension(ext)
            .ok_or_else(|| CompileError::UnsupportedExtension(ext.to_string()))?;

        // 2. Handle interpreted languages (e.g. Python)
        if ext.eq_ignore_ascii_case("py") {
            let python_bin = self
                .resolve_compiler_bin("python3")
                .or_else(|_| self.resolve_compiler_bin("python"))
                .map_err(|_| CompileError::CompilerNotFound("python3/python".to_string()))?;

            let mut py_cmd = Command::new(&python_bin);
            py_cmd.arg("-m").arg("py_compile").arg(source_path);

            let (status, _stdout, stderr) =
                run_command_with_timeout(py_cmd, Duration::from_secs(10))?;

            if !status.success() {
                let err_msg = truncate_10k(&String::from_utf8_lossy(&stderr));
                return Err(CompileError::Failed(err_msg));
            }

            return Ok(CompileResult {
                binary_path: source_path.to_path_buf(),
                is_interpreted: true,
                interpreter_cmd: Some(vec![
                    python_bin.to_string_lossy().to_string(),
                    source_path.to_string_lossy().to_string(),
                ]),
                compiler_output: String::new(),
                duration_ms: start.elapsed().as_millis() as u64,
                cached: false,
            });
        }

        // 3. Resolve compile command template from lang_cfg
        let compile_template = lang_cfg.compile.as_ref().ok_or_else(|| {
            CompileError::Failed(format!(
                "Language '{lang_id}' has no compile command defined"
            ))
        })?;

        let compiler_cmd_name = &compile_template[0];
        let compiler_bin = self.resolve_compiler_bin(compiler_cmd_name)?;

        // Query compiler version using session cache
        let version_line = self.resolve_compiler_version(&compiler_bin);

        // Dynamically resolve memory_mb, stack_mb, and stack_bytes from problem.toml using session cache
        let (mem_mb, stack_mb, stack_bytes) = self.resolve_problem_limits(problem_dir);

        // 4. Compute Blake3 cache key (including source, flags, version, and headers)
        let mut hasher = blake3::Hasher::new();
        hasher.update(lang_id.as_bytes());
        hasher.update(version_line.as_bytes());
        hasher.update(format!("{mem_mb}:{stack_mb}:{stack_bytes}").as_bytes());
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
        for extra in extra_sources {
            if let Ok(b) = fs::read(extra) {
                hasher.update(extra.to_string_lossy().as_bytes());
                hasher.update(&b);
            }
        }

        // Include all dependencies (headers, nested includes, Rust mod, Go imports)
        let mut dep_files = std::collections::BTreeSet::new();
        let header_exts = &["h", "hpp", "hh", "hxx", "h++", "inc", "cuh", "inl", "tpp"];
        let is_cpp_or_c = lang_id.starts_with("cpp") || lang_id.starts_with("c");
        let is_rust = lang_id.starts_with("rust");
        let is_go = lang_id.starts_with("go");

        if is_cpp_or_c {
            // 1. Transitive #include parsing
            scan_includes_transitive(
                &source_bytes,
                source_path.parent(),
                include_dirs,
                &mut dep_files,
                10,
            );
            // 2. Recursive scan of include dirs & source dir for headers
            if let Some(p) = source_path.parent() {
                scan_directory_recursive(p, header_exts, 2, &mut dep_files);
            }
            for inc in include_dirs {
                scan_directory_recursive(inc, header_exts, 2, &mut dep_files);
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

        for dep in dep_files {
            if let Ok(bytes) = fs::read(&dep) {
                hasher.update(dep.to_string_lossy().as_bytes());
                hasher.update(&bytes);
            }
        }

        let cache_hash = hasher.finalize().to_hex().to_string();

        if !no_cache {
            let fast_cache = self.inner.fast_results.lock();
            if let Some(cached_res) = fast_cache.get(&cache_hash)
                && cached_res.binary_path.exists()
                && fs::metadata(&cached_res.binary_path)
                    .map(|m| m.len() > 0)
                    .unwrap_or(false)
            {
                let mut res = cached_res.clone();
                res.cached = true;
                res.duration_ms = start.elapsed().as_millis() as u64;

                let key = (
                    source_path.to_path_buf(),
                    extra_sources.to_vec(),
                    include_dirs.to_vec(),
                );
                if let Ok(meta) = fs::metadata(source_path)
                    && let Ok(mtime) = meta.modified()
                {
                    let mut precheck = self.inner.mtime_precheck.lock();
                    precheck.insert(key, (mtime, meta.len(), res.clone()));
                }

                return Ok(res);
            }
        }

        let base_cache_dir = problem_dir
            .map(|p| p.join(".uj").join("cache").join("bin"))
            .unwrap_or_else(|| PathBuf::from(".uj/cache/bin"));

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
            let fast_cache = self.inner.fast_results.lock();
            if let Some(cached_res) = fast_cache.get(&cache_hash)
                && cached_res.binary_path.exists()
                && fs::metadata(&cached_res.binary_path)
                    .map(|m| m.len() > 0)
                    .unwrap_or(false)
            {
                let mut res = cached_res.clone();
                res.cached = true;
                res.duration_ms = start.elapsed().as_millis() as u64;
                return Ok(res);
            }
        }
        let cache_dir = base_cache_dir.join(&cache_hash);
        let exe_ext = if cfg!(windows) { ".exe" } else { "" };
        let cached_bin = cache_dir.join(format!("prog{exe_ext}"));

        let is_java = lang_id == "java";
        let java_class_name = source_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Main");
        let java_class_file = cache_dir.join(format!("{java_class_name}.class"));

        let target_artifact = if is_java {
            &java_class_file
        } else {
            &cached_bin
        };

        if !no_cache
            && target_artifact.exists()
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
            let mut fast_cache = self.inner.fast_results.lock();
            fast_cache.insert(cache_hash, res.clone());

            let key = (
                source_path.to_path_buf(),
                extra_sources.to_vec(),
                include_dirs.to_vec(),
            );
            if let Ok(meta) = fs::metadata(source_path)
                && let Ok(mtime) = meta.modified()
            {
                let mut precheck = self.inner.mtime_precheck.lock();
                precheck.insert(key, (mtime, meta.len(), res.clone()));
            }

            return Ok(res);
        }

        fs::create_dir_all(&cache_dir)?;

        static COMPILE_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
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
            return Err(CompileError::Failed(truncated));
        }

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
            let mut fast_cache = self.inner.fast_results.lock();
            fast_cache.insert(cache_hash, res.clone());

            let key = (
                source_path.to_path_buf(),
                extra_sources.to_vec(),
                include_dirs.to_vec(),
            );
            if let Ok(meta) = fs::metadata(source_path)
                && let Ok(mtime) = meta.modified()
            {
                let mut precheck = self.inner.mtime_precheck.lock();
                precheck.insert(key, (mtime, meta.len(), res.clone()));
            }
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
}
