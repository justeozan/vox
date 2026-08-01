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

/// Launch an agent in the ACTIVE project.
pub fn spawn_agent(app: &AppHandle, state: &Arc<AppState>, task: &str) {
    let cwd = state.active_project.lock().unwrap().clone();
    spawn_agent_in(app, state, &cwd, task);
}

/// Launch an agent in an explicit worktree directory (voice-driven
/// prompt_worktree delegation).
pub fn spawn_agent_in(app: &AppHandle, state: &Arc<AppState>, cwd: &str, task: &str) {
    let claude = state.paths.lock().unwrap().claude_cli.clone();
    let Some(claude) = claude else {
        eprintln!("[vox] no claude CLI");
        return;
    };
    let cwd = cwd.to_string();
    update_count(app, state, 1);

    let app = app.clone();
    let state = state.clone();
    let task = task.to_string();
    std::thread::spawn(move || {
        let spawned = Command::new(&claude)
            .args(["--print", "--dangerously-skip-permissions", &task])
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn();

        match spawned {
            Ok(mut child) => {
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
            }
            Err(e) => eprintln!("[vox] agent spawn failed: {e}"),
        }
        update_count(&app, &state, -1);
    });
}
