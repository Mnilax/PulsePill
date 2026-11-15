//! Read-only monitor of T3 Code's local state (`%USERPROFILE%\.t3\userdata\statev2.sqlite`).
//!
//! Port of the macOS T3StateReader / T3Monitor. The status mapping and SQL are kept identical:
//!   needsInput  pending runtime request, waiting secret card in an active run, or an unanswered
//!               link card (or a subagent of the thread needs input)
//!   running     an active run (preparing/starting/running/waiting) or an unheld queued run;
//!               a thread whose live subagents keep working after its own run ended
//!   completed   latest run completed
//!   failed      latest run failed / interrupted / cancelled-after-start
//!   idle        nothing recognisable

use crate::model::*;
use crate::sqlite::{Connection, SqlError};
use std::collections::HashMap;
use std::path::PathBuf;

pub const MAX_THREADS: usize = 10;
const PREVIEW_LIMIT: usize = 140;
const FRESH_DONE_WINDOW_MS: i64 = 30 * 60 * 1000;

macro_rules! active {
    () => {
        "('preparing','starting','running','waiting')"
    };
}

const CANDIDATES_SQL: &str = concat!(
    "SELECT t.thread_id, t.updated_at, t.created_at,
      CASE WHEN json_valid(t.payload_json) THEN json_extract(t.payload_json, '$.lineage.relationshipToParent') END,
      CASE WHEN json_valid(t.payload_json) THEN json_extract(t.payload_json, '$.lineage.parentThreadId') END,
      (SELECT r.status FROM orchestration_v2_projection_runs r
         WHERE r.thread_id = t.thread_id AND r.status IN ", active!(), "
         ORDER BY r.ordinal DESC LIMIT 1),
      (SELECT r.status FROM orchestration_v2_projection_runs r
         WHERE r.thread_id = t.thread_id
           AND r.status <> 'rolled_back'
           AND NOT (r.status = 'queued' AND json_extract(r.payload_json, '$.queueHeld') IS 1)
           AND NOT (r.status = 'cancelled' AND json_extract(r.payload_json, '$.startedAt') IS NULL)
         ORDER BY r.ordinal DESC LIMIT 1),
      (SELECT MAX(COALESCE(r.completed_at, json_extract(r.payload_json, '$.startedAt'), r.requested_at))
         FROM orchestration_v2_projection_runs r WHERE r.thread_id = t.thread_id),
      (SELECT MAX(m.created_at) FROM orchestration_v2_projection_messages m WHERE m.thread_id = t.thread_id),
      (SELECT i.updated_at FROM orchestration_v2_projection_turn_items i
         WHERE i.thread_id = t.thread_id ORDER BY i.ordinal DESC LIMIT 1),
      EXISTS (SELECT 1 FROM orchestration_v2_projection_runtime_requests q
         WHERE q.thread_id = t.thread_id AND q.status = 'pending'),
      -- Active runs first (a handful), then an indexed (thread_id, run_id) probe. Joining from
      -- turn_items instead scans every item of the thread (~250 ms on a 400k-item database).
      EXISTS (SELECT 1 FROM orchestration_v2_projection_runs r
         WHERE r.thread_id = t.thread_id AND r.status IN ", active!(), "
           AND EXISTS (SELECT 1 FROM orchestration_v2_projection_turn_items si
             WHERE si.thread_id = r.thread_id AND si.run_id = r.run_id
               AND si.type = 'secret_request' AND si.status = 'waiting')),
      EXISTS (SELECT 1 FROM orchestration_v2_projection_turn_items li
         WHERE li.thread_id = t.thread_id AND li.type = 'link_request' AND li.status = 'waiting'
           AND NOT EXISTS (SELECT 1 FROM orchestration_v2_projection_messages um
             WHERE um.thread_id = li.thread_id AND um.role = 'user'
               AND json_extract(um.payload_json, '$.createdBy') = 'user'
               AND julianday(um.updated_at) >
                   julianday(COALESCE(json_extract(li.payload_json, '$.startedAt'), li.updated_at)))),
      own.status,
      own.updated_at,
      EXISTS (SELECT 1 FROM orchestration_v2_projection_runs pr
         WHERE pr.run_id = own.run_id AND pr.status IN ", active!(), "),
      (SELECT COUNT(*) FROM orchestration_v2_projection_subagents s
         WHERE s.thread_id = t.thread_id AND s.status IN ('pending','running','waiting')
           AND (EXISTS (SELECT 1 FROM orchestration_v2_projection_runs pr
                  WHERE pr.run_id = s.run_id AND pr.status IN ", active!(), ")
                OR EXISTS (SELECT 1 FROM orchestration_v2_projection_runs cr
                  WHERE cr.thread_id = s.child_thread_id AND cr.status IN ", active!(), ")))
    FROM orchestration_v2_projection_threads t
    LEFT JOIN orchestration_v2_projection_subagents own ON own.subagent_id = (
      SELECT s.subagent_id FROM orchestration_v2_projection_subagents s
      WHERE s.child_thread_id = t.thread_id ORDER BY s.updated_at DESC LIMIT 1)
    WHERE t.archived_at IS NULL AND t.deleted_at IS NULL"
);

const DETAIL_SQL: &str = "SELECT t.title, t.default_provider, p.title,
      CASE WHEN json_valid(t.payload_json) THEN json_extract(t.payload_json, '$.modelSelection.model') END,
      (SELECT pt.driver FROM orchestration_v2_projection_provider_threads pt
         WHERE pt.thread_id = t.thread_id AND pt.driver IS NOT NULL ORDER BY pt.updated_at DESC LIMIT 1),
      (SELECT json_extract(tu.payload_json, '$.model') FROM orchestration_v2_projection_provider_turns tu
         WHERE tu.thread_id = t.thread_id ORDER BY tu.started_at DESC LIMIT 1)
    FROM orchestration_v2_projection_threads t
    LEFT JOIN projection_projects p ON p.project_id = t.project_id
    WHERE t.thread_id = ?1";

const RUN_SQL: &str = concat!(
    "SELECT COALESCE(json_extract(r.payload_json, '$.workStartedAt'), json_extract(r.payload_json, '$.startedAt'), r.requested_at),
      json_extract(r.payload_json, '$.modelSelection.model'),
      COALESCE(r.completed_at, json_extract(r.payload_json, '$.startedAt'), r.requested_at)
    FROM orchestration_v2_projection_runs r
    WHERE r.thread_id = ?1
      AND r.status <> 'rolled_back'
      AND NOT (r.status = 'queued' AND json_extract(r.payload_json, '$.queueHeld') IS 1)
      AND NOT (r.status = 'cancelled' AND json_extract(r.payload_json, '$.startedAt') IS NULL)
    ORDER BY (r.status IN ", active!(), ") DESC, r.ordinal DESC
    LIMIT 1"
);

const SUBAGENT_SQL: &str = "SELECT s.driver, s.started_at, json_extract(s.payload_json, '$.progress'),
      COALESCE(json_extract(s.payload_json, '$.model'), json_extract(s.payload_json, '$.modelSelection.model'))
    FROM orchestration_v2_projection_subagents s
    WHERE s.child_thread_id = ?1
    ORDER BY s.updated_at DESC
    LIMIT 1";

const MESSAGE_SQL: &str = "SELECT m.role, substr(json_extract(m.payload_json, '$.text'), 1, 1200), m.updated_at
    FROM orchestration_v2_projection_messages m
    WHERE m.thread_id = ?1
      AND (m.role = 'assistant'
           OR (m.role = 'user' AND COALESCE(json_extract(m.payload_json, '$.createdBy'), 'user') = 'user'))
      AND length(trim(COALESCE(json_extract(m.payload_json, '$.text'), ''))) > 0
    ORDER BY m.created_at DESC, m.message_id DESC
    LIMIT 1";

const PENDING_SQL: &str = concat!(
    "SELECT kind, a, b, c, d FROM (
      SELECT 0 AS prio, q.created_at AS at, q.kind AS kind,
        json_extract(i.payload_json, '$.prompt') AS a,
        json_extract(i.payload_json, '$.reason') AS b,
        json_extract(i.payload_json, '$.questions[0].question') AS c,
        json_extract(i.payload_json, '$.questions[0].header') AS d
      FROM orchestration_v2_projection_runtime_requests q
      LEFT JOIN orchestration_v2_projection_turn_items i
        ON i.node_id = q.node_id AND i.thread_id = q.thread_id
       AND i.type IN ('approval_request', 'user_input_request')
       AND json_extract(i.payload_json, '$.requestId') = q.runtime_request_id
      WHERE q.thread_id = ?1 AND q.status = 'pending'
      UNION ALL
      SELECT 1, si.updated_at, 'secret_request',
        json_extract(si.payload_json, '$.label'), json_extract(si.payload_json, '$.reason'), NULL, NULL
      FROM orchestration_v2_projection_runs r
      JOIN orchestration_v2_projection_turn_items si ON si.thread_id = r.thread_id AND si.run_id = r.run_id
      WHERE r.thread_id = ?1 AND r.status IN ", active!(), "
        AND si.type = 'secret_request' AND si.status = 'waiting'
      UNION ALL
      SELECT 1, li.updated_at, 'link_request',
        COALESCE(json_extract(li.payload_json, '$.label'), json_extract(li.payload_json, '$.url')),
        json_extract(li.payload_json, '$.reason'), NULL, NULL
      FROM orchestration_v2_projection_turn_items li
      WHERE li.thread_id = ?1 AND li.type = 'link_request' AND li.status = 'waiting'
        AND NOT EXISTS (SELECT 1 FROM orchestration_v2_projection_messages um
          WHERE um.thread_id = li.thread_id AND um.role = 'user'
            AND json_extract(um.payload_json, '$.createdBy') = 'user'
            AND julianday(um.updated_at) >
                julianday(COALESCE(json_extract(li.payload_json, '$.startedAt'), li.updated_at)))
    )
    ORDER BY CASE WHEN prio = 0 AND kind <> 'user_input' THEN 0 ELSE 1 END, at DESC
    LIMIT 1"
);

struct Candidate {
    id: String,
    is_subagent: bool,
    parent_id: Option<String>,
    active_run_status: Option<String>,
    latest_run_status: Option<String>,
    has_pending_request: bool,
    has_pending_secret: bool,
    has_pending_link: bool,
    own_subagent_status: Option<String>,
    own_subagent_run_live: bool,
    live_subagents: i64,
    activity_at: i64,
    state: AgentState,
}

fn run_state(status: &str) -> AgentState {
    match status {
        "preparing" | "queued" | "starting" | "running" | "waiting" => AgentState::Running,
        "completed" => AgentState::Completed,
        "failed" | "interrupted" | "cancelled" => AgentState::Failed,
        _ => AgentState::Idle,
    }
}

fn base_state(c: &Candidate) -> AgentState {
    if c.has_pending_request || c.has_pending_secret || c.has_pending_link {
        return AgentState::NeedsInput;
    }
    if c.active_run_status.is_some() {
        return AgentState::Running;
    }
    if let Some(s) = &c.latest_run_status {
        return run_state(s);
    }
    if c.is_subagent {
        if let Some(s) = &c.own_subagent_status {
            return match s.as_str() {
                "pending" | "running" | "waiting" if c.own_subagent_run_live => AgentState::Running,
                "completed" => AgentState::Completed,
                "failed" | "interrupted" | "cancelled" => AgentState::Failed,
                _ => AgentState::Idle,
            };
        }
    }
    AgentState::Idle
}

/// needs input → freshly done/failed → running → older finished / idle.
fn state_rank(state: AgentState, at: i64, now: i64) -> u8 {
    match state {
        AgentState::NeedsInput => 0,
        AgentState::Completed | AgentState::Failed => if now - at <= FRESH_DONE_WINDOW_MS { 1 } else { 3 },
        AgentState::Running => 2,
        AgentState::Idle => 3,
    }
}

fn date(s: Option<String>) -> Option<i64> {
    s.as_deref().and_then(parse_iso)
}

fn clean(s: Option<String>) -> Option<String> {
    s.and_then(|t| preview(&t, 110))
}

fn pending_summary(kind: &str, a: Option<String>, b: Option<String>, q: Option<String>, h: Option<String>) -> String {
    let (a, b) = (clean(a), clean(b));
    let detail = a.clone().or(b.clone());
    let with = |prefix: &str, fallback: &str| detail.as_ref().map(|d| format!("{prefix}{d}")).unwrap_or_else(|| fallback.into());
    let text = match kind {
        "command" => with("Approve command: ", "Approve a command"),
        "file-change" => with("Approve edit: ", "Approve file changes"),
        "file-read" => with("Approve read: ", "Approve file read"),
        "permission" => with("Approve: ", "Permission requested"),
        "mcp-elicitation" => with("MCP input: ", "MCP server needs input"),
        "user_input" => clean(q).or(clean(h)).map(|x| format!("Question: {x}")).unwrap_or_else(|| "Agent asked a question".into()),
        "dynamic_tool_call" => "Tool call waiting for a response".into(),
        "auth_refresh" => "Sign-in required".into(),
        "secret_request" => a.map(|x| format!("Secret requested: {x}")).unwrap_or_else(|| "Secret requested".into()),
        "link_request" => detail.map(|x| format!("Link environment: {x}")).unwrap_or_else(|| "Link an environment".into()),
        _ => with("Input needed: ", "Waiting for your input"),
    };
    truncate(&text, PREVIEW_LIMIT)
}

fn first_non_empty(values: &[&Option<String>]) -> Option<String> {
    values.iter().find_map(|v| v.as_ref().filter(|s| !s.trim().is_empty()).cloned())
}

pub struct ReadResult {
    pub threads: Vec<AgentThread>,
    pub running_count: usize,
    pub needs_input_count: usize,
}

pub fn read(db: &Connection, server_online: bool) -> Result<ReadResult, SqlError> {
    db.exec("BEGIN")?;
    let result = read_inner(db, server_online);
    let _ = db.exec("COMMIT");
    result
}

fn read_inner(db: &Connection, server_online: bool) -> Result<ReadResult, SqlError> {
    let now = now_ms();
    let mut cands: Vec<Candidate> = Vec::new();
    db.query(CANDIDATES_SQL, &[], |r| {
        let Some(id) = r.text(0) else { return };
        let updated = date(r.text(1));
        let created = date(r.text(2));
        let own_updated = date(r.text(14));
        let activity = [date(r.text(7)), date(r.text(8)), date(r.text(9)), own_updated]
            .into_iter()
            .flatten()
            .max()
            .or(updated)
            .or(created)
            .unwrap_or(0);
        cands.push(Candidate {
            id,
            is_subagent: r.text(3).as_deref() == Some("subagent"),
            parent_id: r.text(4),
            active_run_status: r.text(5),
            latest_run_status: r.text(6),
            has_pending_request: r.bool(10),
            has_pending_secret: r.bool(11),
            has_pending_link: r.bool(12),
            own_subagent_status: r.text(13),
            own_subagent_run_live: r.bool(15),
            live_subagents: r.int(16),
            activity_at: activity,
            state: AgentState::Idle,
        });
    })?;

    let mut pending_source: HashMap<String, String> = HashMap::new();
    if server_online {
        for c in cands.iter_mut() {
            c.state = base_state(c);
        }
        let index: HashMap<String, usize> = cands.iter().enumerate().map(|(i, c)| (c.id.clone(), i)).collect();
        let children: Vec<(String, String)> = cands
            .iter()
            .filter(|c| c.is_subagent && c.state == AgentState::NeedsInput)
            .filter_map(|c| Some((c.parent_id.clone()?, c.id.clone())))
            .collect();
        for (parent, child) in children {
            if let Some(&pi) = index.get(&parent) {
                if cands[pi].state != AgentState::NeedsInput {
                    cands[pi].state = AgentState::NeedsInput;
                    pending_source.insert(parent, child);
                }
            }
        }
        for c in cands.iter_mut() {
            if c.live_subagents > 0 && !matches!(c.state, AgentState::Running | AgentState::NeedsInput) {
                c.state = AgentState::Running;
            }
        }
    } else {
        // Server down: nothing can be running or answerable; fall back to the last terminal status.
        for c in cands.iter_mut() {
            let s = c.latest_run_status.as_deref().map(run_state).unwrap_or(AgentState::Idle);
            c.state = if s == AgentState::Running { AgentState::Idle } else { s };
            c.live_subagents = 0;
        }
    }

    let top = cands.iter().filter(|c| !c.is_subagent);
    let running_count = top.clone().filter(|c| c.state == AgentState::Running).count();
    let needs_input_count = top.filter(|c| c.state == AgentState::NeedsInput).count();

    let mut selected: Vec<&Candidate> = cands
        .iter()
        .filter(|c| !c.is_subagent || matches!(c.state, AgentState::Running | AgentState::NeedsInput))
        .collect();
    selected.sort_by(|a, b| {
        (state_rank(a.state, a.activity_at, now), std::cmp::Reverse(a.activity_at), &a.id)
            .cmp(&(state_rank(b.state, b.activity_at, now), std::cmp::Reverse(b.activity_at), &b.id))
    });
    selected.truncate(MAX_THREADS);

    let mut threads = Vec::with_capacity(selected.len());
    for c in selected {
        threads.push(load_thread(db, c, pending_source.get(&c.id).map(String::as_str))?);
    }
    threads.sort_by(|a, b| {
        (state_rank(a.state, a.updated_at, now), std::cmp::Reverse(a.updated_at), &a.id)
            .cmp(&(state_rank(b.state, b.updated_at, now), std::cmp::Reverse(b.updated_at), &b.id))
    });
    Ok(ReadResult { threads, running_count, needs_input_count })
}

fn load_pending(db: &Connection, id: &str) -> Result<Option<String>, SqlError> {
    let mut out = None;
    db.query(PENDING_SQL, &[id], |r| {
        out = Some(pending_summary(&r.text(0).unwrap_or_default(), r.text(1), r.text(2), r.text(3), r.text(4)));
    })?;
    Ok(out)
}

fn load_thread(db: &Connection, c: &Candidate, pending_source: Option<&str>) -> Result<AgentThread, SqlError> {
    let id = c.id.as_str();
    let (mut title, mut provider, mut project, mut thread_model, mut driver, mut turn_model) =
        (String::new(), String::new(), None, None, None, None);
    db.query(DETAIL_SQL, &[id], |r| {
        title = r.text(0).unwrap_or_default();
        provider = r.text(1).unwrap_or_default();
        project = r.text(2);
        thread_model = r.text(3);
        driver = r.text(4);
        turn_model = r.text(5);
    })?;

    let (mut run_started, mut run_model, mut run_activity) = (None, None, None);
    db.query(RUN_SQL, &[id], |r| {
        run_started = date(r.text(0));
        run_model = r.text(1);
        run_activity = date(r.text(2));
    })?;

    let (mut sub_progress, mut sub_model) = (None, None);
    if c.is_subagent {
        db.query(SUBAGENT_SQL, &[id], |r| {
            if driver.is_none() {
                driver = r.text(0);
            }
            if run_started.is_none() {
                run_started = date(r.text(1));
            }
            sub_progress = r.text(2);
            sub_model = r.text(3);
        })?;
    }

    let (mut msg_text, mut msg_at) = (None, None);
    db.query(MESSAGE_SQL, &[id], |r| {
        msg_text = r.text(1);
        msg_at = date(r.text(2));
    })?;

    let live = matches!(c.state, AgentState::Running | AgentState::NeedsInput);
    let mut prev = if c.is_subagent && live { sub_progress.as_deref().and_then(|p| preview(p, PREVIEW_LIMIT)) } else { None };
    if prev.is_none() {
        prev = msg_text.as_deref().and_then(|m| preview(m, PREVIEW_LIMIT));
    }

    let mut pending = None;
    if c.state == AgentState::NeedsInput {
        pending = load_pending(db, id)?;
        if pending.is_none() {
            if let Some(child) = pending_source {
                pending = load_pending(db, child)?.map(|s| format!("Subagent: {s}"));
            }
        }
        pending.get_or_insert_with(|| "Waiting for your input".into());
    }

    let updated_at = [Some(c.activity_at), run_activity, msg_at].into_iter().flatten().max().unwrap_or(c.activity_at);
    let title = title.trim();
    let project = project.map(|p| p.trim().to_string()).filter(|p| !p.is_empty() && p != "No project");
    Ok(AgentThread {
        id: c.id.clone(),
        title: if title.is_empty() { "Untitled thread".into() } else { title.to_string() },
        provider: driver.unwrap_or(if provider.is_empty() { "unknown".into() } else { provider }),
        model: first_non_empty(&[&turn_model, &run_model, &sub_model, &thread_model]),
        project_name: project,
        state: c.state,
        updated_at,
        run_started_at: run_started,
        last_message_preview: prev,
        pending_request_summary: pending,
        subagent_count: c.live_subagents,
        is_subagent: c.is_subagent,
    })
}

// MARK: - Monitor (file stamps + server liveness)

pub struct Paths {
    pub database: PathBuf,
    pub wal: PathBuf,
    pub runtime: PathBuf,
    pub environment_id: PathBuf,
}

impl Paths {
    pub fn resolve() -> Paths {
        let base = std::env::var("T3CODE_HOME")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default()).join(".t3"));
        let dir = base.join("userdata");
        Paths {
            database: dir.join("statev2.sqlite"),
            wal: dir.join("statev2.sqlite-wal"),
            runtime: dir.join("server-runtime.json"),
            environment_id: dir.join("environment-id"),
        }
    }
}

fn stamp(p: &PathBuf) -> (u64, u128) {
    std::fs::metadata(p)
        .map(|m| (m.len(), m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0)))
        .unwrap_or((0, 0))
}

fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            // Access denied still means the process exists.
            return windows_sys::Win32::Foundation::GetLastError() == windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code) != 0 && code == STILL_ACTIVE as u32;
        CloseHandle(h);
        ok
    }
}

/// Tiny extraction of `"pid": 123` without a JSON dependency.
fn read_pid(text: &str) -> Option<u32> {
    let at = text.find("\"pid\"")? + 5;
    let rest = text[at..].trim_start().strip_prefix(':')?.trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok().filter(|&p| p > 0)
}

pub struct Monitor {
    paths: Paths,
    conn: Option<Connection>,
    last_sig: Option<((u64, u128), (u64, u128), (u64, u128), bool)>,
    last: AgentsSnapshot,
    last_refresh_ms: i64,
    failed: bool,
}

impl Monitor {
    pub fn new() -> Monitor {
        Monitor { paths: Paths::resolve(), conn: None, last_sig: None, last: AgentsSnapshot::default(), last_refresh_ms: 0, failed: false }
    }

    fn server_online(&self) -> bool {
        std::fs::read_to_string(&self.paths.runtime).ok().as_deref().and_then(read_pid).map(pid_alive).unwrap_or(false)
    }

    /// Called about once per second. Queries only when something changed (or every 15 s).
    pub fn poll(&mut self) -> Option<AgentsSnapshot> {
        let online = self.server_online();
        let sig = (stamp(&self.paths.database), stamp(&self.paths.wal), stamp(&self.paths.runtime), online);
        let now = now_ms();
        let stale = now - self.last_refresh_ms > 15_000;
        if !self.failed && !stale && self.last_sig == Some(sig) {
            return None;
        }
        self.last_sig = Some(sig);
        self.last_refresh_ms = now;

        let mut snap = AgentsSnapshot {
            server_online: online,
            environment_id: std::fs::read_to_string(&self.paths.environment_id).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
            ..Default::default()
        };
        if sig.0 .0 == 0 {
            self.conn = None;
            self.failed = true;
            snap.last_error = Some("T3 Code database not found".into());
        } else {
            if self.conn.is_none() {
                match Connection::open_read_only(&self.paths.database.to_string_lossy()) {
                    Ok(c) => self.conn = Some(c),
                    Err(e) => snap.last_error = Some(format!("Cannot open T3 database: {}", e.message)),
                }
            }
            if let Some(conn) = &self.conn {
                match read(conn, online) {
                    Ok(r) => {
                        self.failed = false;
                        snap.threads = r.threads;
                        snap.running_count = r.running_count;
                        snap.needs_input_count = r.needs_input_count;
                        if !online {
                            snap.last_error = Some("T3 Code server is not running".into());
                        }
                    }
                    Err(e) => {
                        self.failed = true;
                        if e.is_transient() {
                            snap.threads = self.last.threads.clone();
                            snap.running_count = self.last.running_count;
                            snap.needs_input_count = self.last.needs_input_count;
                        } else {
                            self.conn = None;
                        }
                        snap.last_error = Some(if e.message.starts_with("no such") {
                            format!("Unrecognised T3 database schema ({})", e.message)
                        } else {
                            format!("Cannot read T3 database: {}", e.message)
                        });
                    }
                }
            } else {
                self.failed = true;
            }
        }
        if snap == self.last {
            return None;
        }
        self.last = snap.clone();
        Some(snap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pid() {
        assert_eq!(read_pid(r#"{"version":1,"pid":11180,"host":"x"}"#), Some(11180));
        assert_eq!(read_pid(r#"{"pid": 0}"#), None);
    }
}
