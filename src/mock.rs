//! Sample data for `--mock` (screenshots and UI work without T3 Code).

use crate::model::*;

fn t(id: &str, title: &str, model: &str, project: Option<&str>, state: AgentState, ago_s: i64, preview: &str, pending: Option<&str>, subs: i64) -> AgentThread {
    let now = now_ms();
    AgentThread {
        id: id.into(),
        title: title.into(),
        provider: "claudeAgent".into(),
        model: Some(model.into()),
        project_name: project.map(Into::into),
        state,
        updated_at: now - ago_s * 1000,
        run_started_at: Some(now - ago_s * 1000 - 95_000),
        last_message_preview: Some(preview.into()),
        pending_request_summary: pending.map(Into::into),
        subagent_count: subs,
        is_subagent: false,
    }
}

pub fn agents(scenario: &str) -> AgentsSnapshot {
    use AgentState::*;
    let threads = match scenario {
        "idle" => vec![],
        "calm" => vec![
            t("a", "Refactor settings screen", "claude-sonnet-5-5", Some("web-app"), Completed, 240, "Done: split SettingsView into three sections.", None, 0),
            t("b", "Fix flaky login test", "gpt-5.2-codex", Some("api"), Running, 20, "Running the suite again with retries disabled", None, 0),
        ],
        _ => vec![
            t("a", "Port PulsePill to Windows", "claude-opus-5-5", Some("PulsePill"), NeedsInput, 5, "Ready to build the release binary.", Some("Approve command: cargo build --release"), 0),
            t("b", "Write migration for invoices", "claude-sonnet-5-5", Some("billing"), Completed, 90, "Migration 0042 added; tests pass.", None, 0),
            t("c", "Investigate memory leak in worker", "claude-opus-5-5", Some("queue"), Running, 3, "Heap snapshots show retained closures in retry()", None, 2),
            t("d", "Update README screenshots", "gpt-5.2-codex", None, Running, 40, "Rendering the dark theme variant", None, 0),
            t("e", "Bump dependencies", "claude-haiku-5-5", Some("web-app"), Failed, 600, "npm ERR! peer dependency conflict", None, 0),
        ],
    };
    let running_count = threads.iter().filter(|x| x.state == AgentState::Running).count();
    let needs_input_count = threads.iter().filter(|x| x.state == AgentState::NeedsInput).count();
    AgentsSnapshot { threads, running_count, needs_input_count, server_online: scenario != "idle", last_error: None, environment_id: None }
}

pub fn system(tick: u64, scenario: &str) -> SystemSnapshot {
    let hot = scenario == "hot";
    let wave = |i: u64, k: f64| (((i as f64) * k).sin() * 0.5 + 0.5) * if hot { 0.4 } else { 0.35 } + if hot { 0.55 } else { 0.12 };
    let cpu_history: Vec<f64> = (0..60).map(|i| wave(i + tick, 0.31)).collect();
    let gpu_history: Vec<f64> = (0..60).map(|i| wave(i + tick, 0.17) * 0.8).collect();
    SystemSnapshot {
        cpu_usage: *cpu_history.last().unwrap(),
        gpu_usage: gpu_history.last().copied(),
        cpu_history,
        gpu_history,
        memory_used: 11_400_000_000,
        memory_total: 17_179_869_184,
        memory_load: if hot { 0.91 } else { 0.66 },
        battery_level: Some(if hot { 0.14 } else { 0.78 }),
        is_charging: false,
        is_plugged_in: false,
        battery_minutes: Some(184),
        net_down: 1_450_000.0,
        net_up: 96_000.0,
        self_memory: 0,
    }
}
