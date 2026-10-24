use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Serialize, Deserialize)]
pub struct DoctorCheckItem {
    pub name: String,
    pub status: CheckStatus,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DoctorReport {
    pub schema_version: u32,
    pub ok: bool,
    pub platform: String,
    pub checks: Vec<DoctorCheckItem>,
}

/// Returns actionable instructions and profile content for installing an AppArmor profile
/// allowing `uj` to create unprivileged user namespaces on Ubuntu 23.10+.
pub fn apparmor_profile_instructions() -> String {
    let exe_path = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "/usr/local/bin/uj".to_string());
    format!(
        "Note: Installing an AppArmor profile requires root privileges; uj does not modify /etc/apparmor.d automatically.\n\
Run the following command as root to install the profile for '{exe_path}':\n\n\
sudo tee /etc/apparmor.d/uj >/dev/null <<'EOF'\n\
abi <abi/4.0>,\n\
include <tunables/global>\n\
\n\
profile uj {exe_path} flags=(unconfined) {{\n\
  userns,\n\
}}\n\
EOF\n\
sudo apparmor_parser -r /etc/apparmor.d/uj\n\n\
Alternatively, disable the restriction system-wide:\n\
sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0"
    )
}

pub fn run_doctor(problem_dir: Option<&Path>, fix_apparmor: bool) -> DoctorReport {
    let mut checks = Vec::new();

    if fix_apparmor {
        eprintln!("{}", apparmor_profile_instructions());
    }

    #[cfg(target_os = "windows")]
    {
        run_windows_checks(&mut checks);
    }

    #[cfg(target_os = "linux")]
    {
        run_linux_checks(&mut checks, fix_apparmor);
    }

    if let Some(dir) = problem_dir {
        run_problem_checks(&mut checks, dir);
    }

    let ok = checks.iter().all(|c| c.status != CheckStatus::Fail);

    DoctorReport {
        schema_version: 1,
        ok,
        platform: std::env::consts::OS.to_string(),
        checks,
    }
}

#[cfg(target_os = "windows")]
fn run_windows_checks(checks: &mut Vec<DoctorCheckItem>) {
    // 1. Job Object
    match uj_sandbox::windows::job::JobObject::create(None) {
        Ok(_) => {
            checks.push(DoctorCheckItem {
                name: "Windows Job Objects".to_string(),
                status: CheckStatus::Pass,
                message: "Job Object creation and limit controls operational".to_string(),
                recommendation: None,
            });
        }
        Err(e) => {
            checks.push(DoctorCheckItem {
                name: "Windows Job Objects".to_string(),
                status: CheckStatus::Fail,
                message: format!("Failed to create Job Object: {e}"),
                recommendation: Some(
                    "Check Windows group policies or user permissions".to_string(),
                ),
            });
        }
    }

    // 2. AppContainer
    match uj_sandbox::windows::security::AppContainerProfile::get_or_create(None) {
        Ok(profile) => {
            let sid_str = profile
                .sid_string()
                .unwrap_or_else(|_| "unknown".to_string());
            checks.push(DoctorCheckItem {
                name: "Windows AppContainer (Tier Full)".to_string(),
                status: CheckStatus::Pass,
                message: format!("AppContainer profile active (SID: {sid_str})"),
                recommendation: None,
            });
        }
        Err(e) => {
            checks.push(DoctorCheckItem {
                name: "Windows AppContainer (Tier Full)".to_string(),
                status: CheckStatus::Warn,
                message: format!(
                    "AppContainer unavailable ({e}); system will operate in Degraded tier"
                ),
                recommendation: Some(
                    "AppContainer requires Windows 10/11 or Windows Server 2016+".to_string(),
                ),
            });
        }
    }

    // 3. Defender Recommendation
    checks.push(DoctorCheckItem {
        name: "Windows Defender Latency".to_string(),
        status: CheckStatus::Warn,
        message: "Real-time antivirus scanning may introduce 50-200ms cold startup overhead on new binaries".to_string(),
        recommendation: Some("Add the problem workspace directory and .uj/ to Windows Defender exclusions for optimal judging latency".to_string()),
    });

    // 4. Power Plan / Throttling Check
    checks.push(DoctorCheckItem {
        name: "CPU Frequency & Power Plan".to_string(),
        status: CheckStatus::Pass,
        message: "Ensure laptop is plugged in with 'High Performance' power mode for minimum timing jitter".to_string(),
        recommendation: None,
    });
}

#[cfg(target_os = "linux")]
fn run_linux_checks(checks: &mut Vec<DoctorCheckItem>, fix_apparmor: bool) {
    // 1. Kernel version >= 5.19
    let uname = std::process::Command::new("uname").arg("-r").output();
    if let Ok(out) = uname {
        let release = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let parts: Vec<&str> = release.split('.').collect();
        if parts.len() >= 2 {
            let major: u32 = parts[0].parse().unwrap_or(0);
            let minor: u32 = parts[1]
                .split('-')
                .next()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0);
            if major > 5 || (major == 5 && minor >= 19) {
                checks.push(DoctorCheckItem {
                    name: "Linux Kernel Version".to_string(),
                    status: CheckStatus::Pass,
                    message: format!(
                        "Linux {release} (>= 5.19 required for memory.peak and cgroup.kill)"
                    ),
                    recommendation: None,
                });
            } else {
                checks.push(DoctorCheckItem {
                    name: "Linux Kernel Version".to_string(),
                    status: CheckStatus::Fail,
                    message: format!("Linux {release} is below minimum requirement 5.19"),
                    recommendation: Some(
                        "Please upgrade kernel to >= 5.19 for safe cgroup v2 judging".to_string(),
                    ),
                });
            }
        }
    }

    // 2. cgroup v2
    let cgroup_v2 = Path::new("/sys/fs/cgroup/cgroup.controllers").exists();
    if cgroup_v2 {
        checks.push(DoctorCheckItem {
            name: "cgroup v2 Unified Hierarchy".to_string(),
            status: CheckStatus::Pass,
            message: "cgroup v2 mounted at /sys/fs/cgroup".to_string(),
            recommendation: None,
        });
    } else {
        checks.push(DoctorCheckItem {
            name: "cgroup v2 Unified Hierarchy".to_string(),
            status: CheckStatus::Fail,
            message: "cgroup v2 unified hierarchy not found".to_string(),
            recommendation: Some("Boot kernel with systemd.unified_cgroup_hierarchy=1".to_string()),
        });
    }

    // 3. Unprivileged user namespaces
    let max_userns_path = Path::new("/proc/sys/user/max_user_namespaces");
    let userns_enabled = if max_userns_path.exists() {
        std::fs::read_to_string(max_userns_path)
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(0)
            > 0
    } else {
        true
    };

    if userns_enabled {
        checks.push(DoctorCheckItem {
            name: "Unprivileged User Namespaces".to_string(),
            status: CheckStatus::Pass,
            message: "max_user_namespaces enabled".to_string(),
            recommendation: None,
        });
    } else {
        checks.push(DoctorCheckItem {
            name: "Unprivileged User Namespaces".to_string(),
            status: CheckStatus::Fail,
            message: "max_user_namespaces is disabled (0)".to_string(),
            recommendation: Some("Run 'sudo sysctl -w user.max_user_namespaces=15000'".to_string()),
        });
    }

    // 4. AppArmor restrictions (Ubuntu 23.10+)
    let apparmor_restrict = Path::new("/proc/sys/kernel/apparmor_restrict_unprivileged_userns");
    if apparmor_restrict.exists() {
        let val = std::fs::read_to_string(apparmor_restrict).unwrap_or_default();
        if val.trim() == "1" {
            let rec = if fix_apparmor {
                apparmor_profile_instructions()
            } else {
                "Run 'sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0' or view AppArmor profile installation instructions via 'uj doctor --fix-apparmor'".to_string()
            };
            checks.push(DoctorCheckItem {
                name: "Ubuntu AppArmor Userns Restriction".to_string(),
                status: CheckStatus::Warn,
                message: "kernel.apparmor_restrict_unprivileged_userns=1 is active".to_string(),
                recommendation: Some(rec),
            });
        }
    }
}

fn run_problem_checks(checks: &mut Vec<DoctorCheckItem>, root: &Path) {
    let problem_toml = root.join("problem.toml");
    if !problem_toml.exists() {
        checks.push(DoctorCheckItem {
            name: "Problem Configuration".to_string(),
            status: CheckStatus::Warn,
            message: "No problem.toml found in current directory".to_string(),
            recommendation: Some("Initialize problem structure with 'uj init <dir>'".to_string()),
        });
        return;
    }

    let content = match std::fs::read_to_string(&problem_toml) {
        Ok(c) => c,
        Err(e) => {
            checks.push(DoctorCheckItem {
                name: "Problem Configuration".to_string(),
                status: CheckStatus::Fail,
                message: format!("Failed to read problem.toml: {e}"),
                recommendation: None,
            });
            return;
        }
    };

    match uj_core::config::ProblemConfig::from_toml_str(&content, Some(&problem_toml)) {
        Ok(cfg) => {
            checks.push(DoctorCheckItem {
                name: "Problem Configuration".to_string(),
                status: CheckStatus::Pass,
                message: format!(
                    "Loaded problem '{}' (type: {:?})",
                    cfg.name, cfg.problem_type
                ),
                recommendation: None,
            });

            // Check test inputs
            match uj_core::tests::discover_tests(root, &cfg) {
                Ok(discovered) => {
                    let count = discovered.len();
                    checks.push(DoctorCheckItem {
                        name: "Test Cases Discovery".to_string(),
                        status: if count > 0 {
                            CheckStatus::Pass
                        } else {
                            CheckStatus::Warn
                        },
                        message: format!("Discovered {count} test cases"),
                        recommendation: if count == 0 {
                            Some(
                                "Generate test cases using 'uj gen' or place files in tests/"
                                    .to_string(),
                            )
                        } else {
                            None
                        },
                    });

                    // 1. Check .in / .ans pairing
                    let mut missing_ans = Vec::new();
                    for t in &discovered {
                        if t.answer_path.is_none() {
                            missing_ans.push(t.name.clone());
                        }
                    }
                    if missing_ans.is_empty() {
                        checks.push(DoctorCheckItem {
                            name: "Test Input/Answer Pairing".to_string(),
                            status: CheckStatus::Pass,
                            message: "All test inputs have matching .ans files".to_string(),
                            recommendation: None,
                        });
                    } else {
                        checks.push(DoctorCheckItem {
                            name: "Test Input/Answer Pairing".to_string(),
                            status: CheckStatus::Warn,
                            message: format!(
                                "Missing .ans for {} test(s): {}",
                                missing_ans.len(),
                                missing_ans.join(", ")
                            ),
                            recommendation: Some(
                                "Generate answer files using 'uj ans'".to_string(),
                            ),
                        });
                    }

                    // 1b. Check for orphan .ans / .out files without matching .in
                    let mut orphan_ans = Vec::new();
                    let known_in_stems: std::collections::HashSet<std::path::PathBuf> = discovered
                        .iter()
                        .map(|t| t.input_path.with_extension(""))
                        .collect();
                    let tests_dir = root.join(&cfg.tests.dir);
                    if let Ok(entries) = std::fs::read_dir(&tests_dir) {
                        for entry in entries.flatten() {
                            let p = entry.path();
                            if p.is_file()
                                && let Some(ext) = p.extension().and_then(|e| e.to_str())
                                && (ext == "ans" || ext == "out")
                            {
                                let stem_path = p.with_extension("");
                                if !known_in_stems.contains(&stem_path) {
                                    orphan_ans.push(
                                        p.file_name()
                                            .unwrap_or_default()
                                            .to_string_lossy()
                                            .to_string(),
                                    );
                                }
                            }
                        }
                    }
                    orphan_ans.sort();
                    if orphan_ans.is_empty() {
                        checks.push(DoctorCheckItem {
                            name: "Orphan Answer Files".to_string(),
                            status: CheckStatus::Pass,
                            message: "No orphan .ans/.out files without matching .in files"
                                .to_string(),
                            recommendation: None,
                        });
                    } else {
                        checks.push(DoctorCheckItem {
                            name: "Orphan Answer Files".to_string(),
                            status: CheckStatus::Warn,
                            message: format!(
                                "Orphan answer files without .in: {}",
                                orphan_ans.join(", ")
                            ),
                            recommendation: Some(
                                "Remove stale .ans/.out files or add their corresponding .in files"
                                    .to_string(),
                            ),
                        });
                    }

                    // 2. Check for duplicate test data (streaming hash) and answer cache freshness
                    let mut hashes: std::collections::HashMap<String, String> =
                        std::collections::HashMap::new();
                    let mut duplicates = Vec::new();
                    let ans_cache_path = root.join(".uj").join("cache").join("ans_state.json");
                    let ans_cache: Option<std::collections::BTreeMap<String, String>> =
                        std::fs::read_to_string(&ans_cache_path)
                            .ok()
                            .and_then(|s| serde_json::from_str(&s).ok());
                    let mut stale_ans = Vec::new();

                    for t in &discovered {
                        if let Ok(hash) = crate::author_steps::hash_file_streaming(&t.input_path) {
                            if let Some(ref cache_map) = ans_cache
                                && let Some(cached_hash) = cache_map.get(&t.name)
                                && cached_hash != &hash
                            {
                                stale_ans.push(t.name.clone());
                            }
                            if let Some(existing) = hashes.get(&hash) {
                                duplicates.push(format!("'{}' == '{}'", t.name, existing));
                            } else {
                                hashes.insert(hash, t.name.clone());
                            }
                        }
                    }
                    if duplicates.is_empty() {
                        checks.push(DoctorCheckItem {
                            name: "Test Data Uniqueness".to_string(),
                            status: CheckStatus::Pass,
                            message: "No duplicate input files detected".to_string(),
                            recommendation: None,
                        });
                    } else {
                        checks.push(DoctorCheckItem {
                            name: "Test Data Uniqueness".to_string(),
                            status: CheckStatus::Warn,
                            message: format!(
                                "Duplicate test data found: {}",
                                duplicates.join(", ")
                            ),
                            recommendation: Some(
                                "Remove redundant duplicate test inputs".to_string(),
                            ),
                        });
                    }

                    if !stale_ans.is_empty() {
                        checks.push(DoctorCheckItem {
                            name: "Answer Cache Freshness".to_string(),
                            status: CheckStatus::Warn,
                            message: format!(
                                "Input modified since .ans was generated for: {}",
                                stale_ans.join(", ")
                            ),
                            recommendation: Some(
                                "Run 'uj ans --force' to regenerate stale answer files".to_string(),
                            ),
                        });
                    }

                    // 3. Check subtask coverage and pattern overlap
                    if !cfg.subtasks.is_empty() {
                        let uncovered: Vec<_> = discovered
                            .iter()
                            .filter(|t| t.subtask_id.is_none())
                            .map(|t| t.name.as_str())
                            .collect();
                        let empty_subtasks: Vec<u32> = cfg
                            .subtasks
                            .iter()
                            .filter(|st| !discovered.iter().any(|t| t.subtask_id == Some(st.id)))
                            .map(|st| st.id)
                            .collect();
                        if uncovered.is_empty() && empty_subtasks.is_empty() {
                            checks.push(DoctorCheckItem {
                                name: "Subtask Coverage".to_string(),
                                status: CheckStatus::Pass,
                                message:
                                    "All test cases are covered and all subtasks are non-empty"
                                        .to_string(),
                                recommendation: None,
                            });
                        } else {
                            let mut msgs = Vec::new();
                            if !uncovered.is_empty() {
                                msgs.push(format!(
                                    "Tests not assigned to any subtask: {}",
                                    uncovered.join(", ")
                                ));
                            }
                            if !empty_subtasks.is_empty() {
                                msgs.push(format!(
                                    "Subtasks matching 0 test cases: {:?}",
                                    empty_subtasks
                                ));
                            }
                            checks.push(DoctorCheckItem {
                                name: "Subtask Coverage".to_string(),
                                status: CheckStatus::Warn,
                                message: msgs.join("; "),
                                recommendation: Some(
                                    "Update subtask tests pattern in problem.toml".to_string(),
                                ),
                            });
                        }

                        let overlaps =
                            uj_core::tests::detect_subtask_pattern_overlaps(&discovered, &cfg);
                        if overlaps.is_empty() {
                            checks.push(DoctorCheckItem {
                                name: "Subtask Pattern Overlap".to_string(),
                                status: CheckStatus::Pass,
                                message: "No test cases match multiple subtask patterns"
                                    .to_string(),
                                recommendation: None,
                            });
                        } else {
                            let desc: Vec<String> = overlaps
                                .iter()
                                .map(|(name, ids)| format!("'{name}' -> subtasks {ids:?}"))
                                .collect();
                            checks.push(DoctorCheckItem {
                                name: "Subtask Pattern Overlap".to_string(),
                                status: CheckStatus::Warn,
                                message: format!(
                                    "Test cases matched by multiple subtasks: {}",
                                    desc.join(", ")
                                ),
                                recommendation: Some(
                                    "Make [[subtasks]] test glob patterns mutually exclusive"
                                        .to_string(),
                                ),
                            });
                        }
                    }

                    // 4. Check checker and validator compilation
                    if let Some(ref chk_path) = cfg.checker.path {
                        let full_p = if chk_path.is_absolute() {
                            chk_path.clone()
                        } else {
                            root.join(chk_path)
                        };
                        match uj_engine::compiler::Compiler::compile(&full_p, false) {
                            Ok(_) => {
                                checks.push(DoctorCheckItem {
                                    name: "Checker Compilation".to_string(),
                                    status: CheckStatus::Pass,
                                    message: format!(
                                        "Checker '{}' compiles cleanly",
                                        chk_path.display()
                                    ),
                                    recommendation: None,
                                });
                            }
                            Err(e) => {
                                checks.push(DoctorCheckItem {
                                    name: "Checker Compilation".to_string(),
                                    status: CheckStatus::Fail,
                                    message: format!("Failed to compile checker: {e}"),
                                    recommendation: Some(
                                        "Fix syntax/compiler errors in checker".to_string(),
                                    ),
                                });
                            }
                        }
                    }

                    if let Some(ref val_path) = cfg.validator {
                        let full_p = if val_path.is_absolute() {
                            val_path.clone()
                        } else {
                            root.join(val_path)
                        };
                        match uj_engine::compiler::Compiler::compile(&full_p, false) {
                            Ok(_) => {
                                checks.push(DoctorCheckItem {
                                    name: "Validator Compilation".to_string(),
                                    status: CheckStatus::Pass,
                                    message: format!(
                                        "Validator '{}' compiles cleanly",
                                        val_path.display()
                                    ),
                                    recommendation: None,
                                });
                            }
                            Err(e) => {
                                checks.push(DoctorCheckItem {
                                    name: "Validator Compilation".to_string(),
                                    status: CheckStatus::Fail,
                                    message: format!("Failed to compile validator: {e}"),
                                    recommendation: Some(
                                        "Fix syntax/compiler errors in validator".to_string(),
                                    ),
                                });
                            }
                        }
                    }
                }
                Err(e) => {
                    checks.push(DoctorCheckItem {
                        name: "Test Cases Discovery".to_string(),
                        status: CheckStatus::Fail,
                        message: format!("Error discovering test cases: {e}"),
                        recommendation: None,
                    });
                }
            }
        }
        Err(e) => {
            checks.push(DoctorCheckItem {
                name: "Problem Configuration".to_string(),
                status: CheckStatus::Fail,
                message: format!("problem.toml parse error: {e}"),
                recommendation: Some(
                    "Check problem.toml syntax according to schema ('uj schema config')"
                        .to_string(),
                ),
            });
        }
    }
}
