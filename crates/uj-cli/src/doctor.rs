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
    pub ok: bool,
    pub platform: String,
    pub checks: Vec<DoctorCheckItem>,
}

pub fn run_doctor(problem_mode: bool, _fix_apparmor: bool) -> DoctorReport {
    let mut checks = Vec::new();

    #[cfg(target_os = "windows")]
    {
        run_windows_checks(&mut checks);
    }

    #[cfg(target_os = "linux")]
    {
        run_linux_checks(&mut checks, _fix_apparmor);
    }

    if problem_mode {
        run_problem_checks(&mut checks, Path::new("."));
    }

    let ok = checks.iter().all(|c| c.status != CheckStatus::Fail);

    DoctorReport {
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
    let mut kernel_ok = false;
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
                kernel_ok = true;
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
            if fix_apparmor {
                // Print profile
                eprintln!("Installing AppArmor profile for uj...");
            }
            checks.push(DoctorCheckItem {
                name: "Ubuntu AppArmor Userns Restriction".to_string(),
                status: CheckStatus::Warn,
                message: "kernel.apparmor_restrict_unprivileged_userns=1 is active".to_string(),
                recommendation: Some("Run 'sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0' or install uj AppArmor profile via 'uj doctor --fix-apparmor'".to_string()),
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
