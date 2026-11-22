//! `--mode rpc --session <path>` / `--resume <path>` bind that session file (temp HOME, no
//! provider call): rpc used to ignore both and open a fresh file in the default session dir.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::Value;

fn temp_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let agent = home.path().join(".omp/agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("models.yml"),
        "providers:\n  mock:\n    baseUrl: http://127.0.0.1:9/v1\n    apiKey: unused\n    api: openai-completions\n    models:\n      - id: m\n        name: M\n        contextWindow: 8000\n        maxTokens: 100\n",
    )
    .unwrap();
    std::fs::create_dir_all(home.path().join("work")).unwrap();
    home
}

/// Run `omt --mode rpc <extra>` for one `get_state` and return its `data`.
fn rpc_state(home: &Path, extra: &[&str]) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_omt"))
        .env("HOME", home)
        .env_remove("OMP_AGENT_DIR")
        .env_remove("OMP_AUTH_BROKER_URL")
        .env_remove("OMP_AUTH_BROKER_TOKEN")
        .env("NO_COLOR", "1")
        .current_dir(home.join("work"))
        .args(["--mode", "rpc", "--no-extensions", "--model", "mock/m"])
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"id\":\"s1\",\"type\":\"get_state\"}\n")
        .unwrap();
    // stdin closes here, which ends the rpc loop after the reply.
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout: {stdout}\nstderr: {stderr}");
    stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["type"] == "response" && v["id"] == "s1")
        .unwrap_or_else(|| panic!("no get_state response\nstdout: {stdout}\nstderr: {stderr}"))
        ["data"]
        .clone()
}

fn session_files(root: &Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return found;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            found.extend(session_files(&p));
        } else if p.extension().is_some_and(|x| x == "jsonl") {
            found.push(p);
        }
    }
    found
}

#[test]
fn rpc_binds_new_session_path_then_resumes_it() {
    let home = temp_home();
    let lane = home.path().join("lanes/lane.jsonl");
    let lane_arg = lane.display().to_string();

    let first = rpc_state(home.path(), &["--session", &lane_arg]);
    assert_eq!(first["sessionFile"], lane_arg.as_str(), "{first}");
    assert!(lane.is_file(), "requested session file was not created");
    let id = first["sessionId"].clone();
    assert!(id.is_string(), "{first}");
    assert!(
        session_files(&home.path().join(".omp-rs")).is_empty(),
        "rpc also opened a session in the default dir"
    );

    // The same file again, through both spellings, is the same session.
    for flag in ["--session", "--resume"] {
        let again = rpc_state(home.path(), &[flag, &lane_arg]);
        assert_eq!(again["sessionFile"], lane_arg.as_str(), "{flag}: {again}");
        assert_eq!(again["sessionId"], id, "{flag}: {again}");
    }
}

#[test]
fn rpc_without_session_flag_keeps_default_session_dir() {
    let home = temp_home();
    let state = rpc_state(home.path(), &[]);
    let file = state["sessionFile"].as_str().unwrap();
    assert!(
        Path::new(file).starts_with(home.path().join(".omp-rs")),
        "{state}"
    );
}
