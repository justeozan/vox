//! Claude CLI agent subprocesses with a hard timeout, mirrored to the
//! renderer badge via `agent-status` events.

use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tauri::{AppHandle, Emitter};

use crate::AppState;

const AGENT_TIMEOUT: Duration = Duration::from_secs(120);

fn update_count(app: &AppHandle, state: &Arc<AppState>, delta: i64) {
    let mut count = state.agent_count.fetch_add(delta, Ordering::SeqCst) + delta;
    if count < 0 {
        state.agent_count.store(0, Ordering::SeqCst);
        count = 0;
    }
    let _ = app.emit("agent-status", json!({ "count": count }));
}

pub fn spawn_agent(app: &AppHandle, state: &Arc<AppState>, task: &str) {
    let claude = state.paths.lock().unwrap().claude_cli.clone();
    let Some(claude) = claude else {
        eprintln!("[vox] no claude CLI");
        return;
    };
    let cwd = state.active_project.lock().unwrap().clone();
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
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => break,
                        Ok(None) => {
                            if start.elapsed() > AGENT_TIMEOUT {
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
