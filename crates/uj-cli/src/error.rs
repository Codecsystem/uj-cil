#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::process::ExitCode;
use uj_core::config::ConfigErrorInfo;

pub const EXIT_SUCCESS: u8 = 0;
pub const EXIT_NON_AC: u8 = 1;
pub const EXIT_USAGE_OR_CONFIG: u8 = 2;
pub const EXIT_JUDGE_FAILED: u8 = 3;
pub const EXIT_VERIFY_MISMATCH: u8 = 4;
pub const EXIT_COMPILE_ERROR: u8 = 5;
pub const EXIT_INTERRUPTED: u8 = 130;

#[derive(Debug, Serialize, Deserialize)]
pub struct StructuredErrorEnvelope {
    pub ok: bool,
    pub error: ConfigErrorInfo,
}

impl StructuredErrorEnvelope {
    pub fn new(error: ConfigErrorInfo) -> Self {
        Self { ok: false, error }
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
