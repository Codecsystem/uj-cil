//! Source code compilation, language detection, and compilation caching.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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

#[derive(Default)]
struct CompileSessionInner {
    compiler_bins: parking_lot::Mutex<std::collections::HashMap<String, Result<PathBuf, String>>>,
    compiler_versions: parking_lot::Mutex<std::collections::HashMap<PathBuf, String>>,
    languages_configs: parking_lot::Mutex<
        std::collections::HashMap<Option<PathBuf>, uj_core::config::LanguagesConfig>,
    >,
    problem_limits: parking_lot::Mutex<std::collections::HashMap<PathBuf, (u64, u64, u64)>>,
    fast_results: parking_lot::Mutex<std::collections::HashMap<FastCompileKey, FastCompileValue>>,
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
        let version_output = Command::new(bin)
            .arg("--version")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        let version_line = version_output
            .lines()
            .next()
            .unwrap_or("unknown")
            .to_string();
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
        let source_bytes = fs::read(source_path)?;

        let problem_dir = source_path
            .ancestors()
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

            let py_compile = Command::new(&python_bin)
                .arg("-m")
                .arg("py_compile")
                .arg(source_path)
                .output()?;

            if !py_compile.status.success() {
                let err_msg = truncate_10k(&String::from_utf8_lossy(&py_compile.stderr));
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

        // Include headers in include dirs and problem dir
        for inc in include_dirs {
            hasher.update(inc.to_string_lossy().as_bytes());
            if inc.is_dir()
                && let Ok(entries) = fs::read_dir(inc)
            {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|e| e == "h" || e == "hpp")
                        && let Ok(bytes) = fs::read(&path)
                    {
                        hasher.update(path.to_string_lossy().as_bytes());
                        hasher.update(&bytes);
                    }
                }
            }
        }

        if let Some(src_parent) = source_path.parent()
            && src_parent.is_dir()
            && let Ok(entries) = fs::read_dir(src_parent)
        {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "h" || e == "hpp")
                    && let Ok(bytes) = fs::read(&path)
                {
                    hasher.update(path.to_string_lossy().as_bytes());
                    hasher.update(&bytes);
                }
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
                return Ok(res);
            }
        }

        let base_cache_dir = problem_dir
            .map(|p| p.join(".uj").join("cache").join("bin"))
            .unwrap_or_else(|| PathBuf::from(".uj/cache/bin"));
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
            return Ok(res);
        }

        fs::create_dir_all(&cache_dir)?;

        // 5. Construct compiler command arguments
        let mut final_args = Vec::new();
        for arg in &compile_template[1..] {
            match arg.as_str() {
                "{exe}" => final_args.push(cached_bin.to_string_lossy().to_string()),
                "{src}" => final_args.push(source_path.to_string_lossy().to_string()),
                "{dir}" => final_args.push(cache_dir.to_string_lossy().to_string()),
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

        // 6. Spawn compiler process with concurrent reader threads to prevent pipe buffer deadlock
        let mut cmd = Command::new(&compiler_bin);
        cmd.args(&final_args);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        let timeout = Duration::from_secs(30);

        let mut stdout_pipe = child.stdout.take().unwrap();
        let mut stderr_pipe = child.stderr.take().unwrap();

        let stdout_thread = std::thread::spawn(move || {
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
            buf
        });

        let stderr_thread = std::thread::spawn(move || {
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
            buf
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
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }

        let stdout_bytes = stdout_thread.join().unwrap_or_default();
        let stderr_bytes = stderr_thread.join().unwrap_or_default();

        if timed_out {
            return Err(CompileError::Failed(
                "Compilation timed out (30s limit exceeded)".to_string(),
            ));
        }

        let status = exit_status.unwrap_or_else(|| child.wait().unwrap());
        let duration_ms = start.elapsed().as_millis() as u64;

        let combined_output = format!(
            "{}{}",
            String::from_utf8_lossy(&stdout_bytes),
            String::from_utf8_lossy(&stderr_bytes)
        );

        if !status.success() {
            let truncated = truncate_10k(&combined_output);
            return Err(CompileError::Failed(truncated));
        }

        if !target_artifact.exists()
            || fs::metadata(target_artifact)
                .map(|m| m.len() == 0)
                .unwrap_or(true)
        {
            return Err(CompileError::Failed(
                "Compiler succeeded but produced no valid binary file".to_string(),
            ));
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
}
