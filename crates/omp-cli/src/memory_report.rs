//! `memory_report`: the gateway's memory snapshot of this process.
//!
//! The report is a JSON file under `<agent dir>/memory-reports/`; the answer is
//! `{path, footprintBytes}`. The gateway copies the file off the machine and removes it, and reads
//! `main.footprint.bytes` out of it. Alongside the process's own counters the report carries the
//! sessions the process hosts (count and messages) and the RSS of each session's extension host.
//! A platform without a memory source answers the coded error `unsupported`; no number is guessed.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use omp_procmem::{MemoryError, ProcessMemory};
use serde_json::{json, Value};

use crate::runtime::{CmdError, CmdResult, Runtime};

const SCHEMA: &str = "omt.memory-report/1";

/// What one hosted session adds to the report.
pub(crate) struct SessionSample {
    pub session_id: String,
    pub messages: usize,
    /// Process id of the session's private extension host, when it has one.
    pub ext_host_pid: Option<u32>,
}

impl SessionSample {
    pub(crate) async fn of(rt: &Arc<Runtime>, session_id: String) -> Self {
        Self {
            session_id,
            messages: rt.message_count().await,
            ext_host_pid: match rt.ext_host() {
                Some(host) => host.child_pid().await,
                None => None,
            },
        }
    }
}

/// Where this process leaves its reports.
fn report_dir() -> PathBuf {
    omp_providers::AgentDir::default().0.join("memory-reports")
}

/// Measure this process, write the report and answer `{path, footprintBytes}`.
pub(crate) async fn handle(samples: Vec<SessionSample>) -> CmdResult {
    let dir = report_dir();
    tokio::task::spawn_blocking(move || take_report(&samples, &dir))
        .await
        .map_err(|e| CmdError::coded(format!("memory report task: {e}"), "memory_report_failed"))?
}

fn take_report(samples: &[SessionSample], dir: &Path) -> CmdResult {
    let memory = omp_procmem::current_process().map_err(memory_error)?;
    let hosts: Vec<ExtHostRss> = samples
        .iter()
        .filter_map(|s| {
            Some(ExtHostRss {
                session_id: &s.session_id,
                pid: s.ext_host_pid?,
                rss_bytes: omp_procmem::process_rss_bytes(s.ext_host_pid?).ok(),
            })
        })
        .collect();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let report = build_report(now_ms, std::process::id(), &memory, samples, &hosts);
    let path = write_report(dir, now_ms, &report)?;
    Ok(Some(json!({
        "path": path.display().to_string(),
        "footprintBytes": memory.footprint_bytes,
    })))
}

fn memory_error(e: MemoryError) -> CmdError {
    match e {
        MemoryError::Unsupported { .. } => CmdError::coded(e.to_string(), "unsupported"),
        MemoryError::Unreadable(_) => CmdError::coded(e.to_string(), "memory_unreadable"),
    }
}

struct ExtHostRss<'a> {
    session_id: &'a str,
    pid: u32,
    /// `None` when the host exited or its size could not be read.
    rss_bytes: Option<u64>,
}

fn build_report(
    now_ms: u128,
    pid: u32,
    memory: &ProcessMemory,
    samples: &[SessionSample],
    hosts: &[ExtHostRss<'_>],
) -> Value {
    let smaps: Value = memory
        .smaps_rollup
        .as_ref()
        .map_or(Value::Null, |counters| {
            Value::Object(
                counters
                    .iter()
                    .map(|(k, v)| (k.clone(), json!(v)))
                    .collect(),
            )
        });
    json!({
        "schema": SCHEMA,
        "generatedAtMs": u64::try_from(now_ms).unwrap_or(u64::MAX),
        "pid": pid,
        "platform": std::env::consts::OS,
        "engine": crate::rpc::ENGINE,
        "serverVersion": env!("CARGO_PKG_VERSION"),
        "main": {
            "footprint": {"bytes": memory.footprint_bytes, "source": memory.footprint_source},
            "rss": {"bytes": memory.rss_bytes},
            "peakRss": {"bytes": memory.peak_rss_bytes},
            "smapsRollupBytes": smaps,
        },
        "sessions": {
            "count": samples.len(),
            "messages": samples.iter().map(|s| s.messages).sum::<usize>(),
            "items": samples
                .iter()
                .map(|s| json!({"sessionId": s.session_id, "messages": s.messages}))
                .collect::<Vec<_>>(),
        },
        "extensionHosts": {
            "count": hosts.len(),
            "rssBytes": hosts.iter().filter_map(|h| h.rss_bytes).sum::<u64>(),
            "items": hosts
                .iter()
                .map(|h| json!({"sessionId": h.session_id, "pid": h.pid, "rssBytes": h.rss_bytes}))
                .collect::<Vec<_>>(),
        },
    })
}

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Write `report` to a new private file in `dir` and return its path.
fn write_report(dir: &Path, now_ms: u128, report: &Value) -> Result<PathBuf, CmdError> {
    let failed = |what: &str, e: &dyn std::fmt::Display| {
        CmdError::coded(format!("{what}: {e}"), "memory_report_failed")
    };
    std::fs::create_dir_all(dir).map_err(|e| failed(&dir.display().to_string(), &e))?;
    let name = format!(
        "memory-report-{}-{now_ms}-{}.json",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let path = dir.join(name);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .map_err(|e| failed(&path.display().to_string(), &e))?;
    let text = serde_json::to_string_pretty(report).map_err(|e| failed("serialize", &e))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .map_err(|e| failed(&path.display().to_string(), &e))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> ProcessMemory {
        ProcessMemory {
            footprint_bytes: 4096,
            footprint_source: "VmRSS",
            rss_bytes: 4096,
            peak_rss_bytes: Some(8192),
            smaps_rollup: Some(vec![("Rss".to_owned(), 4096), ("Pss".to_owned(), 2048)]),
        }
    }

    fn sample(id: &str, messages: usize, pid: Option<u32>) -> SessionSample {
        SessionSample {
            session_id: id.to_owned(),
            messages,
            ext_host_pid: pid,
        }
    }

    #[test]
    fn the_report_carries_footprint_sessions_messages_and_extension_host_rss() {
        let samples = [sample("rpc-1", 2, Some(77)), sample("rpc-2", 5, None)];
        let hosts = [ExtHostRss {
            session_id: "rpc-1",
            pid: 77,
            rss_bytes: Some(1_000),
        }];
        let report = build_report(1_700_000_000_123, 42, &memory(), &samples, &hosts);
        assert_eq!(report["schema"], SCHEMA);
        assert_eq!(report["pid"], 42);
        assert_eq!(report["main"]["footprint"]["bytes"], 4096);
        assert_eq!(report["main"]["footprint"]["source"], "VmRSS");
        assert_eq!(report["main"]["peakRss"]["bytes"], 8192);
        assert_eq!(report["main"]["smapsRollupBytes"]["Pss"], 2048);
        assert_eq!(report["sessions"]["count"], 2);
        assert_eq!(report["sessions"]["messages"], 7);
        assert_eq!(report["sessions"]["items"][1]["sessionId"], "rpc-2");
        assert_eq!(report["extensionHosts"]["count"], 1);
        assert_eq!(report["extensionHosts"]["rssBytes"], 1_000);
        assert_eq!(report["extensionHosts"]["items"][0]["pid"], 77);
    }

    #[test]
    fn a_host_that_cannot_be_read_counts_as_unknown_not_zero() {
        let hosts = [ExtHostRss {
            session_id: "rpc-1",
            pid: 9,
            rss_bytes: None,
        }];
        let report = build_report(0, 1, &memory(), &[sample("rpc-1", 0, Some(9))], &hosts);
        assert_eq!(report["extensionHosts"]["count"], 1);
        assert_eq!(
            report["extensionHosts"]["items"][0]["rssBytes"],
            Value::Null
        );
        assert_eq!(report["extensionHosts"]["rssBytes"], 0);
    }

    #[test]
    fn a_missing_smaps_rollup_is_null() {
        let mut memory = memory();
        memory.smaps_rollup = None;
        let report = build_report(0, 1, &memory, &[], &[]);
        assert_eq!(report["main"]["smapsRollupBytes"], Value::Null);
        assert_eq!(report["sessions"]["count"], 0);
    }

    #[test]
    fn the_answer_names_the_written_file_and_its_footprint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let answer = take_report(&[sample("rpc-1", 3, None)], &dir.path().join("reports"));
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let data = answer.expect("answered").expect("data");
            let path = PathBuf::from(data["path"].as_str().expect("path"));
            assert!(path.starts_with(dir.path().join("reports")));
            let report: Value =
                serde_json::from_str(&std::fs::read_to_string(&path).expect("report file"))
                    .expect("report is JSON");
            assert_eq!(report["main"]["footprint"]["bytes"], data["footprintBytes"]);
            assert!(data["footprintBytes"].as_u64().is_some_and(|b| b > 0));
            assert_eq!(report["sessions"]["messages"], 3);
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        assert_eq!(answer.expect_err("unsupported").code, Some("unsupported"));
    }

    #[test]
    fn two_reports_in_one_millisecond_do_not_overwrite_each_other() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = write_report(dir.path(), 5, &json!({"n": 1})).expect("first");
        let second = write_report(dir.path(), 5, &json!({"n": 2})).expect("second");
        assert_ne!(first, second);
    }

    #[cfg(unix)]
    #[test]
    fn the_report_is_readable_by_its_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_report(dir.path(), 1, &json!({})).expect("report");
        let mode = std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0);
    }

    #[test]
    fn memory_errors_keep_their_codes() {
        assert_eq!(
            memory_error(MemoryError::Unsupported { os: "plan9" }).code,
            Some("unsupported")
        );
        assert_eq!(
            memory_error(MemoryError::Unreadable("x".into())).code,
            Some("memory_unreadable")
        );
    }
}
