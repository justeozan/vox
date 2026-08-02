//! Claude CLI agent subprocesses with a hard timeout, mirrored to the
//! renderer badge via `agent-status` events.

use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tauri::{AppHandle, Emitter};

use crate::AppState;

/// Real coding work takes minutes, not seconds. Overridable for demos via
/// VOX_AGENT_TIMEOUT (seconds).
fn agent_timeout() -> Duration {
    let secs = std::env::var("VOX_AGENT_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(600);
    Duration::from_secs(secs)
}

fn update_count(app: &AppHandle, state: &Arc<AppState>, delta: i64) {
    let mut count = state.agent_count.fetch_add(delta, Ordering::SeqCst) + delta;
    if count < 0 {
        state.agent_count.store(0, Ordering::SeqCst);
        count = 0;
    }
    let _ = app.emit("agent-status", json!({ "count": count }));
}

/// Launch an agent in the ACTIVE project. Returns whether it actually started.
pub fn spawn_agent(app: &AppHandle, state: &Arc<AppState>, task: &str) -> bool {
    let cwd = state.active_project.lock().unwrap().clone();
    let name = std::path::Path::new(&cwd)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| cwd.clone());
    spawn_agent_in(app, state, &name, &cwd, task)
}

/// Launch an agent in an explicit worktree directory (voice-driven
/// prompt_worktree delegation). Spawns SYNCHRONOUSLY so the caller learns
/// whether it started; only the wait-for-exit loop is backgrounded. Returns
/// false (and emits `agent-error`) when the CLI is missing or the spawn fails —
/// so Vox never confirms a delegation that never happened. On success emits the
/// `delegation` echo with the exact prompt.
pub fn spawn_agent_in(
    app: &AppHandle,
    state: &Arc<AppState>,
    project: &str,
    cwd: &str,
    task: &str,
) -> bool {
    let claude = state.paths.lock().unwrap().claude_cli.clone();
    let Some(claude) = claude else {
        eprintln!("[vox] no claude CLI — cannot launch agent");
        let _ = app.emit("agent-error", json!({ "message": "Claude CLI not found — no agent launched" }));
        return false;
    };
    println!("[vox] launching agent in {cwd}\n         prompt: {task}");

    // `--` so a drafted prompt starting with '-' (bullet lists!) can't be
    // parsed as a CLI flag.
    let spawned = Command::new(&claude)
        .args(["--print", "--dangerously-skip-permissions", "--", task])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();

    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[vox] agent spawn failed: {e}");
            let _ = app.emit("agent-error", json!({ "message": format!("agent launch failed: {e}") }));
            return false;
        }
    };

    // Really launched — now (and only now) count it and echo the prompt.
    update_count(app, state, 1);
    let _ = app.emit(
        "delegation",
        json!({ "project": project, "path": cwd, "prompt": task }),
    );

    let app = app.clone();
    let state = state.clone();
    std::thread::spawn(move || {
        if let Some(stderr) = child.stderr.take() {
            std::thread::spawn(move || {
                use std::io::{BufRead, BufReader};
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let trimmed: String = line.chars().take(200).collect();
                    eprintln!("[agent] {trimmed}");
                }
            });
        }
        let start = Instant::now();
        let timeout = agent_timeout();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if start.elapsed() > timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
                Err(_) => break,
            }
        }
        update_count(&app, &state, -1);
    });
    true
}
