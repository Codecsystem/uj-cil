//! Source code compilation, language detection, and compilation caching.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

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
            other => Err(CompileError::UnsupportedExtension(other.to_string())),
        }
    }

    pub fn id(&self) -> &'static str {
        match self {
            Self::Cpp => "cpp",
            Self::C => "c",
            Self::Rust => "rust",
            Self::Python => "python",
        }
    }
}

pub struct Compiler;

impl Compiler {
    /// Compile source file, utilizing .uj/cache/bin/<blake3> caching.
    pub fn compile(source_path: &Path, no_cache: bool) -> Result<CompileResult, CompileError> {
        let lang = Language::from_path(source_path)?;
        let source_bytes = fs::read(source_path)?;

        let start = Instant::now();

        if lang == Language::Python {
            // Python is interpreted, verify syntax with py_compile
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

        // Determine compiler and default flags
        let (compiler_name, default_args) = match lang {
            Language::Cpp => (
                "g++",
                vec![
                    "-O3",
                    "-std=c++17",
                    "-Wall",
                    "-Wextra",
                    #[cfg(windows)]
                    "-Wl,--stack,268435456",
                    #[cfg(windows)]
                    "-static",
                ],
            ),
            Language::C => (
                "gcc",
                vec![
                    "-O3",
                    "-Wall",
                    #[cfg(windows)]
                    "-Wl,--stack,268435456",
                    #[cfg(windows)]
                    "-static",
                ],
            ),
            Language::Rust => ("rustc", vec!["-O"]),
            Language::Python => unreachable!(),
        };

        let compiler_bin = which::which(compiler_name)
            .map_err(|_| CompileError::CompilerNotFound(compiler_name.to_string()))?;

        // Query compiler version for cache key uniqueness
        let version_output = Command::new(&compiler_bin)
            .arg("--version")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        let version_line = version_output.lines().next().unwrap_or("unknown");

        // Compute Blake3 cache key
        let mut hasher = blake3::Hasher::new();
        hasher.update(lang.id().as_bytes());
        hasher.update(version_line.as_bytes());
        for arg in &default_args {
            hasher.update(arg.as_bytes());
        }
        hasher.update(&source_bytes);
        let cache_hash = hasher.finalize().to_hex().to_string();

        let cache_dir = PathBuf::from(".uj/cache/bin").join(&cache_hash);
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

        // Perform compilation
        fs::create_dir_all(&cache_dir)?;

        let mut cmd = Command::new(&compiler_bin);
        for arg in &default_args {
            cmd.arg(arg);
        }
        cmd.arg(source_path);
        cmd.arg("-o");
        cmd.arg(&cached_bin);

        let output = cmd.output()?;
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
        format!("{}\n... [Output truncated to 10 KiB]", &s[..LIMIT])
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
}
