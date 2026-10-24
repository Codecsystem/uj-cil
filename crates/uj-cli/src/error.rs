#![allow(dead_code)]

use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crossbeam_channel::{Receiver, Sender, unbounded};
use serde::{Deserialize, Serialize};
use uj_core::config::{ConfigErrorInfo, ProblemConfig};
use uj_sandbox::CancelToken;

use crate::cli::OutputFormat;

pub const EXIT_SUCCESS: u8 = 0;
pub const EXIT_NON_AC: u8 = 1;
pub const EXIT_USAGE_OR_CONFIG: u8 = 2;
pub const EXIT_JUDGE_FAILED: u8 = 3;
pub const EXIT_VERIFY_MISMATCH: u8 = 4;
pub const EXIT_COMPILE_ERROR: u8 = 5;
pub const EXIT_INTERRUPTED: u8 = 130;

struct CtrlcRegistry {
    tokens: Vec<CancelToken>,
    waiters: Vec<Sender<()>>,
}

static CTRLC_REGISTRY: OnceLock<Mutex<CtrlcRegistry>> = OnceLock::new();

fn ensure_ctrlc_registry() -> &'static Mutex<CtrlcRegistry> {
    CTRLC_REGISTRY.get_or_init(|| {
        let reg = Mutex::new(CtrlcRegistry {
            tokens: Vec::new(),
            waiters: Vec::new(),
        });
        let _ = ctrlc::set_handler(|| {
            if let Some(m) = CTRLC_REGISTRY.get()
                && let Ok(mut guard) = m.lock()
            {
                for token in &guard.tokens {
                    token.cancel();
                }
                guard.waiters.retain(|tx| tx.send(()).is_ok());
            }
        });
        reg
    })
}

/// Install or subscribe to the process-wide Ctrl-C handler and return a new `CancelToken`.
pub fn install_ctrlc() -> CancelToken {
    let token = CancelToken::new();
    let reg = ensure_ctrlc_registry();
    if let Ok(mut guard) = reg.lock() {
        guard.tokens.push(token.clone());
    }
    token
}

/// Subscribe to Ctrl-C notifications via both a `CancelToken` and a wakeup `Receiver<()>`.
pub fn subscribe_ctrlc() -> (CancelToken, Receiver<()>) {
    let token = CancelToken::new();
    let (tx, rx) = unbounded();
    let reg = ensure_ctrlc_registry();
    if let Ok(mut guard) = reg.lock() {
        guard.tokens.push(token.clone());
        guard.waiters.push(tx);
    }
    (token, rx)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StructuredErrorEnvelope {
    pub schema_version: u32,
    pub ok: bool,
    pub error: ConfigErrorInfo,
}

impl StructuredErrorEnvelope {
    pub fn new(error: ConfigErrorInfo) -> Self {
        Self {
            schema_version: 1,
            ok: false,
            error,
        }
    }
}

pub fn handle_config_error(err: ConfigErrorInfo, is_json: bool) -> ExitCode {
    if is_json {
        let envelope = StructuredErrorEnvelope::new(err);
        if let Ok(json) = serde_json::to_string_pretty(&envelope) {
            println!("{json}");
        }
    } else {
        eprintln!("Error: {err}");
    }
    ExitCode::from(EXIT_USAGE_OR_CONFIG)
}

pub fn handle_config_error_fmt(err: ConfigErrorInfo, format: OutputFormat) -> ExitCode {
    match format {
        OutputFormat::Json => {
            let envelope = StructuredErrorEnvelope::new(err);
            if let Ok(json) = serde_json::to_string_pretty(&envelope) {
                println!("{json}");
            }
        }
        OutputFormat::Ndjson => {
            let envelope = StructuredErrorEnvelope::new(err);
            if let Ok(json) = serde_json::to_string(&envelope) {
                println!("{json}");
            }
        }
        OutputFormat::Human => {
            eprintln!("Error: {err}");
        }
        OutputFormat::Silent => {}
    }
    ExitCode::from(EXIT_USAGE_OR_CONFIG)
}

/// Emit a structured error envelope in JSON/NDJSON modes or human message on stderr.
pub fn emit_error(
    format: OutputFormat,
    kind: &str,
    message: impl Into<String>,
    hint: Option<&str>,
) {
    let msg = message.into();
    let mut err_map = serde_json::Map::new();
    err_map.insert(
        "kind".to_string(),
        serde_json::Value::String(kind.to_string()),
    );
    err_map.insert(
        "message".to_string(),
        serde_json::Value::String(msg.clone()),
    );
    if let Some(h) = hint {
        err_map.insert("hint".to_string(), serde_json::Value::String(h.to_string()));
    }
    let envelope = serde_json::json!({
        "schema_version": 1,
        "ok": false,
        "error": err_map,
    });

    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
        }
        OutputFormat::Ndjson => {
            println!("{}", serde_json::to_string(&envelope).unwrap());
        }
        OutputFormat::Human => {
            eprintln!("Error: {msg}");
            if let Some(h) = hint {
                eprintln!("Hint: {h}");
            }
        }
        OutputFormat::Silent => {}
    }
}

/// Emit a compilation error in JSON/NDJSON/Human format and return `EXIT_COMPILE_ERROR`.
pub fn emit_compile_error(
    format: OutputFormat,
    target: Option<&Path>,
    message: impl Into<String>,
) -> ExitCode {
    let msg = message.into();
    let mut err_map = serde_json::Map::new();
    err_map.insert(
        "kind".to_string(),
        serde_json::Value::String("CompileError".to_string()),
    );
    if let Some(t) = target {
        err_map.insert(
            "target".to_string(),
            serde_json::Value::String(t.to_string_lossy().to_string()),
        );
    }
    err_map.insert(
        "message".to_string(),
        serde_json::Value::String(msg.clone()),
    );
    let envelope = serde_json::json!({
        "schema_version": 1,
        "ok": false,
        "error": err_map,
    });

    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
        }
        OutputFormat::Ndjson => {
            println!("{}", serde_json::to_string(&envelope).unwrap());
        }
        OutputFormat::Human => {
            if let Some(t) = target {
                eprintln!("Failed to compile {}: {msg}", t.display());
            } else {
                eprintln!("{msg}");
            }
        }
        OutputFormat::Silent => {}
    }
    ExitCode::from(EXIT_COMPILE_ERROR)
}

/// Emit an interrupted error envelope and return `EXIT_INTERRUPTED`.
pub fn emit_interrupted(format: OutputFormat, context: &str) -> ExitCode {
    let msg = format!("{context} interrupted");
    match format {
        OutputFormat::Json | OutputFormat::Ndjson => {
            emit_error(format, "Interrupted", msg, None);
        }
        OutputFormat::Human => {
            eprintln!("{msg}");
        }
        OutputFormat::Silent => {}
    }
    ExitCode::from(EXIT_INTERRUPTED)
}

/// Load and validate `problem.toml` from `problem_dir`, emitting structured errors on failure.
pub fn load_problem(
    problem_dir: &Path,
    format: OutputFormat,
) -> Result<(PathBuf, String, ProblemConfig), ExitCode> {
    let config_path = problem_dir.join("problem.toml");
    let content = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) => {
            emit_error(
                format,
                "ConfigError",
                format!(
                    "Cannot read problem.toml at '{}': {e}",
                    config_path.display()
                ),
                Some("Ensure the problem directory exists and contains problem.toml"),
            );
            return Err(ExitCode::from(EXIT_USAGE_OR_CONFIG));
        }
    };
    let cfg = match ProblemConfig::from_toml_str(&content, Some(&config_path)) {
        Ok(c) => c,
        Err(e) => return Err(handle_config_error_fmt(e, format)),
    };
    Ok((config_path, content, cfg))
}

/// Validate a `run_id` parameter to prevent path traversal outside `.uj/runs/`.
/// Returns the sanitized run ID stem (without `.json` extension).
pub fn validate_run_id(run_id: &str) -> Result<String, String> {
    let trimmed = run_id.trim();
    let stem = trimmed.strip_suffix(".json").unwrap_or(trimmed);
    if stem.is_empty() {
        return Err("Invalid run_id: cannot be empty".to_string());
    }
    if stem == "." || stem == ".." || stem.contains("..") {
        return Err(format!(
            "Invalid run_id '{run_id}': path traversal is not allowed"
        ));
    }
    if stem.contains('/') || stem.contains('\\') || stem.contains(':') || stem.contains('\0') {
        return Err(format!(
            "Invalid run_id '{run_id}': path separators are not allowed"
        ));
    }
    let mut components = Path::new(stem).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Ok(stem.to_string()),
        _ => Err(format!(
            "Invalid run_id '{run_id}': must be a single file name identifier"
        )),
    }
}

/// Atomically write bytes to `path` via a temporary file in the same directory + rename.
/// Prevents `SIGBUS` on concurrent mmap readers (e.g. cached `.ans` files) and partial writes.
pub fn atomic_write_bytes(path: &Path, content: &[u8]) -> std::io::Result<()> {
    static TMP_COUNTER: AtomicU64 = AtomicU64::new(1);
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if !parent.as_os_str().is_empty() && !parent.exists() {
        std::fs::create_dir_all(parent)?;
    }

    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("output");
    let pid = std::process::id();
    let seq = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_path = parent.join(format!(".uj_tmp_{file_name}_{pid}_{seq}"));

    std::fs::write(&tmp_path, content)?;

    let mut last_err = None;
    for delay_ms in [0u64, 5, 15, 40, 80] {
        if delay_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        }
        match std::fs::rename(&tmp_path, path) {
            Ok(()) => return Ok(()),
            Err(e) => last_err = Some(e),
        }
    }

    let _ = std::fs::remove_file(&tmp_path);
    Err(last_err.unwrap_or_else(|| std::io::Error::other("atomic rename failed")))
}

/// Print a serializable value to stdout as pretty JSON (in `Json` mode) or single-line JSON (in `Ndjson` mode).
pub fn print_structured<T: Serialize>(format: OutputFormat, val: &T) {
    match format {
        OutputFormat::Json => {
            if let Ok(s) = serde_json::to_string_pretty(val) {
                println!("{s}");
            }
        }
        OutputFormat::Ndjson => {
            if let Ok(s) = serde_json::to_string(val) {
                println!("{s}");
            }
        }
        OutputFormat::Human | OutputFormat::Silent => {}
    }
}
