//! `bash_output` / `kill_bash`: observe and stop background `bash` jobs.
//!
//! A job's output is a rolling buffer, so the read cursor is shared state: without it a second
//! call would replay the whole buffer instead of the delta.

use anyhow::Result;
use pa_agent::types::AgentToolResult;
use serde_json::{json, Value};

use super::jobs::JobStatus;
use super::Env;

const DESCRIPTION: &str = "Read a background bash job's new output. Omit nothing on the first call; each call returns only what arrived since the last one. `filter` is a regex applied per line; `view: \"screen\"` is not available (no PTY).";

const KILL_DESCRIPTION: &str =
    "Stop background bash jobs: `bash_id` for one, `all: true` for every running job.";

#[must_use]
pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "bash_id": {"type": "string", "description": "Job id from the `bash` tool's details.bash_id."},
            "filter": {"type": "string", "description": "Regex; only matching lines are returned. An invalid pattern is ignored."},
            "view": {"type": "string", "enum": ["log"], "description": "Only `log` is supported."},
        },
        "required": ["bash_id"],
        "additionalProperties": false,
    })
}

#[must_use]
pub fn kill_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "bash_id": {"type": "string", "description": "Job id to stop."},
            "all": {"type": "boolean", "description": "Stop every running job."},
        },
        "additionalProperties": false,
    })
}

#[must_use]
pub fn description() -> &'static str {
    DESCRIPTION
}

#[must_use]
pub fn kill_description() -> &'static str {
    KILL_DESCRIPTION
}

fn text(body: String) -> AgentToolResult {
    AgentToolResult::text(body)
}

fn status_head(id: &str, status: JobStatus, rolled_over: bool) -> String {
    let head = match status {
        JobStatus::Running => format!("[{id}] status: running"),
        JobStatus::Completed => format!("[{id}] status: exited 0"),
        JobStatus::Failed => format!("[{id}] status: failed"),
        JobStatus::Killed => format!("[{id}] status: killed"),
    };
    if rolled_over {
        format!("{head}\n[earlier output dropped]")
    } else {
        head
    }
}

/// `bash_output { bash_id, filter?, view? }`.
///
/// # Errors
/// An unknown `bash_id`, or `view` other than `log`.
pub fn bash_output(env: &Env, args: &Value) -> Result<AgentToolResult> {
    let id = args
        .get("bash_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing field `bash_id`"))?;
    if let Some(view) = args.get("view").and_then(Value::as_str) {
        if view != "log" {
            anyhow::bail!("view must be \"log\": no PTY, so there is no screen to render");
        }
    }
    let job = env
        .jobs
        .get(id)
        .ok_or_else(|| anyhow::anyhow!("No terminal session found with id: {id}"))?;

    let pattern = args
        .get("filter")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty());

    let (body, rolled_over) = {
        let buf = job.output.lock().expect("job output tap poisoned");
        let mut at = job.read_at.lock().expect("job read cursor poisoned");
        let rolled_over = *at > buf.len();
        let cursor = if rolled_over { 0 } else { *at };
        let delta = buf[cursor..].to_string();
        *at = buf.len();
        (delta, rolled_over)
    };

    let mut head = status_head(id, job.status, rolled_over);
    let shown = match pattern {
        None => body,
        Some(p) => match regex::Regex::new(p) {
            Ok(re) => body
                .lines()
                .filter(|l| re.is_match(l))
                .collect::<Vec<_>>()
                .join("\n"),
            // An unusable pattern is ignored rather than failing the read.
            Err(_) => body,
        },
    };
    if shown.trim().is_empty() {
        head.push_str("\n(no new output)");
    } else {
        head.push('\n');
        head.push_str(&shown);
    }
    if job.status == JobStatus::Running {
        head.push_str("\n\nmore output may follow; the job is still running.");
    }
    Ok(text(head))
}

/// `kill_bash { bash_id?, all? }`.
///
/// # Errors
/// Neither `bash_id` nor `all`, or an unknown `bash_id`.
pub fn kill_bash(env: &Env, args: &Value) -> Result<AgentToolResult> {
    let all = args.get("all").and_then(Value::as_bool).unwrap_or(false);
    let id = args.get("bash_id").and_then(Value::as_str);
    if all {
        let n = env.jobs.kill_all();
        return Ok(text(format!("Killed {n} session(s).")));
    }
    let Some(id) = id else {
        anyhow::bail!("provide `bash_id` or `all: true`");
    };
    match env.jobs.kill(id) {
        None => anyhow::bail!("No terminal session found with id: {id}"),
        Some(_) => Ok(text(format!("Killed {id}."))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::jobs::Job;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    fn env_with_job(chunks: &[&str], status: JobStatus) -> (Env, String) {
        let env = Env::new(&std::env::temp_dir(), false);
        let id = add_job_with_output(&env, chunks, status);
        (env, id)
    }

    fn add_job(env: &Env, status: JobStatus) -> String {
        add_job_with_output(env, &[], status)
    }

    fn add_job_with_output(env: &Env, chunks: &[&str], status: JobStatus) -> String {
        let output = Arc::new(Mutex::new(String::new()));
        for c in chunks {
            output.lock().unwrap().push_str(c);
        }
        env.jobs.insert(Job {
            command: "test".into(),
            started: Instant::now(),
            status,
            output,
            result: None,
            error: None,
            ended: None,
            read_at: Arc::new(Mutex::new(0)),
            cancel: Arc::new(pa_agent::abort::AbortController::new()),
        })
    }

    fn body(r: &AgentToolResult) -> String {
        r.content
            .iter()
            .filter_map(|c| match c {
                pa_agent::types::ToolResultContent::Text(t) => Some(t.text.clone()),
                pa_agent::types::ToolResultContent::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn read(env: &Env, id: &str, extra: Value) -> String {
        let mut args = json!({"bash_id": id});
        if let Some(o) = extra.as_object() {
            for (k, v) in o {
                args[k] = v.clone();
            }
        }
        body(&bash_output(env, &args).unwrap())
    }

    #[test]
    fn reads_are_incremental() {
        let (env, id) = env_with_job(&["one\n", "two\n"], JobStatus::Running);
        let first = read(&env, &id, Value::Null);
        assert!(first.contains("status: running"), "{first}");
        assert!(first.contains("one\ntwo"), "{first}");
        let second = read(&env, &id, Value::Null);
        assert!(second.contains("(no new output)"), "{second}");
    }

    #[test]
    fn rollover_is_announced() {
        let (env, id) = env_with_job(&["abcdefghij"], JobStatus::Completed);
        env.jobs.update(&id, |j| {
            j.output.lock().unwrap().clear();
            j.output.lock().unwrap().push_str("tail");
            *j.read_at.lock().unwrap() = 10;
        });
        let out = read(&env, &id, Value::Null);
        assert!(out.contains("status: exited"), "{out}");
        assert!(out.contains("earlier output dropped"), "{out}");
        assert!(out.contains("tail"), "{out}");
    }

    #[test]
    fn filter_selects_lines_and_bad_pattern_is_ignored() {
        let (env, id) = env_with_job(&["err: one\nok\n", "err: two\n"], JobStatus::Running);
        let out = read(&env, &id, json!({"filter": "^err"}));
        assert!(out.contains("err: one"), "{out}");
        assert!(out.contains("err: two"), "{out}");
        assert!(!out.contains("ok"), "{out}");
        let (env2, id2) = env_with_job(&["a\nb\n"], JobStatus::Running);
        let out2 = read(&env2, &id2, json!({"filter": "([unclosed"}));
        assert!(out2.contains("a\nb"), "{out2}");
    }

    #[test]
    fn kill_reports_unknown_ids_and_aborts_known_ones() {
        let (env, id) = env_with_job(&[], JobStatus::Running);
        assert!(kill_bash(&env, &json!({})).is_err());
        assert!(kill_bash(&env, &json!({"bash_id": "bg_999"})).is_err());
        let out = body(&kill_bash(&env, &json!({"bash_id": id})).unwrap());
        assert_eq!(out, format!("Killed {id}."));
        assert!(env.jobs.get(&id).unwrap().cancel.signal().is_aborted());
    }

    #[test]
    fn kill_all_counts_only_running_jobs() {
        let (env, done) = env_with_job(&[], JobStatus::Completed);
        let running = add_job(&env, JobStatus::Running);
        let out = body(&kill_bash(&env, &json!({"all": true})).unwrap());
        assert_eq!(out, "Killed 1 session(s).");
        assert!(env.jobs.get(&running).unwrap().cancel.signal().is_aborted());
        assert!(!env.jobs.get(&done).unwrap().cancel.signal().is_aborted());
    }

    #[tokio::test]
    async fn background_job_reads_incrementally_and_dies_on_kill() {
        let env = std::sync::Arc::new(super::super::Env::new(&std::env::temp_dir(), false));
        let started = super::super::bash::bash(
            env.clone(),
            json!({
                "command": "printf 'alpha\\n'; sleep 30",
                "async": true
            }),
            pa_agent::abort::AbortController::new().signal(),
        )
        .await
        .unwrap();
        let id = started.details["bash_id"]
            .as_str()
            .expect("details.bash_id")
            .to_string();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let first = loop {
            let out = body(&bash_output(&env, &json!({"bash_id": id})).unwrap());
            if out.contains("alpha") {
                break out;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no output arrived: {out}"
            );
            tokio::task::yield_now().await;
        };
        assert!(first.contains("status: running"), "{first}");

        let second = body(&bash_output(&env, &json!({"bash_id": id})).unwrap());
        assert!(second.contains("(no new output)"), "{second}");

        body(&kill_bash(&env, &json!({"bash_id": id})).unwrap());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let job = env.jobs.get(&id).expect("job stays registered");
            if job.status == JobStatus::Killed {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "job never reported killed: {:?}",
                job.status
            );
            tokio::task::yield_now().await;
        }
        let after = body(&bash_output(&env, &json!({"bash_id": id})).unwrap());
        assert!(after.contains("status: killed"), "{after}");
    }

    #[test]
    fn screen_view_is_refused_without_a_pty() {
        let (env, id) = env_with_job(&[], JobStatus::Running);
        let err = bash_output(&env, &json!({"bash_id": id, "view": "screen"})).unwrap_err();
        assert!(err.to_string().contains("no PTY"), "{err}");
    }
}
