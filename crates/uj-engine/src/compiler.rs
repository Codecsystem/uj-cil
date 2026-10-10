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
        }
    }
}

pub struct Compiler;

impl Compiler {
    /// Compile source file, utilizing .uj/cache/bin/<blake3> caching.
    pub fn compile(source_path: &Path, no_cache: bool) -> Result<CompileResult, CompileError> {
        Self::compile_with_options(source_path, &[], &[], no_cache)
    }

    /// Compile source file with extra source files (e.g. grader.cpp) and include directories.
    pub fn compile_with_options(
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

        // 1. Load and merge languages.toml
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
                && let Ok(c) =
                    uj_core::config::LanguagesConfig::from_toml_str(&content, Some(&global_cfg_path))
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

        let ext = source_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default();
        let (lang_id, lang_cfg) = lang_configs
            .find_by_extension(ext)
            .ok_or_else(|| CompileError::UnsupportedExtension(ext.to_string()))?;

        // 2. Handle interpreted languages (e.g. Python)
        if ext.eq_ignore_ascii_case("py") {
            let python_bin = which::which("python3")
                .or_else(|_| which::which("python"))
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
            CompileError::Failed(format!("Language '{lang_id}' has no compile command defined"))
        })?;

        let compiler_cmd_name = &compile_template[0];
        let compiler_bin = which::which(compiler_cmd_name)
            .map_err(|_| CompileError::CompilerNotFound(compiler_cmd_name.to_string()))?;

        // Query compiler version for cache key uniqueness
        let version_output = Command::new(&compiler_bin)
            .arg("--version")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        let version_line = version_output.lines().next().unwrap_or("unknown");

        // Dynamically resolve memory_mb, stack_mb, and stack_bytes from problem.toml if available
        let mut mem_mb: u64 = 256;
        let mut stack_mb: u64 = 256;
        let mut stack_bytes: u64 = 268435456;

        if let Some(pdir) = problem_dir {
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
        }

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

        let base_cache_dir = problem_dir
            .map(|p| p.join(".uj").join("cache").join("bin"))
            .unwrap_or_else(|| PathBuf::from(".uj/cache/bin"));
        let cache_dir = base_cache_dir.join(&cache_hash);
        let exe_ext = if cfg!(windows) { ".exe" } else { "" };
        let cached_bin = cache_dir.join(format!("prog{exe_ext}"));

        if !no_cache && cached_bin.exists() {
            return Ok(CompileResult {
                binary_path: cached_bin,
                is_interpreted: false,
                interpreter_cmd: None,
                compiler_output: String::new(),
                duration_ms: start.elapsed().as_millis() as u64,
                cached: true,
            });
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

        // 6. Spawn compiler process with 30s timeout
        let mut cmd = Command::new(&compiler_bin);
        cmd.args(&final_args);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        let timeout = Duration::from_secs(30);
        let mut timed_out = false;

        loop {
            match child.try_wait()? {
                Some(_) => break,
                None => {
                    if start.elapsed() > timeout {
                        timed_out = true;
                        let _ = child.kill();
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }

        if timed_out {
            return Err(CompileError::Failed(
                "Compilation timed out (30s limit exceeded)".to_string(),
            ));
        }

        let output = child.wait_with_output()?;
        let duration_ms = start.elapsed().as_millis() as u64;

        let combined_output = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        if !output.status.success() {
            let truncated = truncate_10k(&combined_output);
            return Err(CompileError::Failed(truncated));
        }

        Ok(CompileResult {
            binary_path: cached_bin,
            is_interpreted: false,
            interpreter_cmd: None,
            compiler_output: truncate_10k(&combined_output),
            duration_ms,
            cached: false,
        })
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
}
