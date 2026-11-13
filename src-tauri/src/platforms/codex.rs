use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};

use crate::database::{
    CachedSessionSummary, SessionContentEntry, SessionContentIndex, SessionSummaryCache,
};

use super::{
    build_commands, content_entries_to_matches, push_bounded_index_entry, tool_text_from_str,
    tool_text_from_value, visit_bounded_jsonl_lines, ContentMatch, PlatformAdapter,
    SessionAgentGroup, SessionDetail, SessionKey, SessionListItem, SessionListResult,
    TimelineBlock, ToolCallBlock, MAX_INDEX_SOURCE_LINE_BYTES,
};

pub struct CodexPlatform {
    codex_home: PathBuf,
    sessions_root: PathBuf,
    project_root: Option<PathBuf>,
}

#[derive(Clone)]
struct SummaryData {
    session_id: String,
    cwd: String,
    preview: String,
    agent: Option<CodexAgentMetadata>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CodexAgentMetadata {
    parent_session_id: Option<String>,
    depth: Option<i32>,
    nickname: Option<String>,
    role: Option<String>,
    path: Option<String>,
}

#[derive(Debug, Clone)]
struct CodexTextPart {
    content_index: usize,
    field_name: String,
    content_type: String,
    text: String,
}

impl CodexPlatform {
    pub fn new(codex_home: PathBuf, project_root: Option<PathBuf>) -> Self {
        Self {
            sessions_root: codex_home.join("sessions"),
            codex_home,
            project_root,
        }
    }

    fn read_jsonl(&self, path: &Path) -> Vec<Value> {
        let raw = fs::read_to_string(path).unwrap_or_default();
        raw.lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect()
    }

    fn scan_summary(&self, path: &Path) -> SummaryData {
        let mut session_id = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("unknown")
            .to_string();
        let mut cwd = String::new();
        let mut preview = String::new();
        let mut agent = None;
        let mut saw_session_meta = false;
        let mut parsed_lines = 0usize;
        let mut found_session_id = false;

        let Ok(file) = File::open(path) else {
            return SummaryData {
                session_id,
                cwd,
                preview,
                agent,
            };
        };
        let reader = BufReader::new(file);

        for line in reader.lines() {
            let Ok(line) = line else { continue };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(parsed) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            parsed_lines += 1;

            let Some(payload) = parsed.get("payload") else {
                continue;
            };

            saw_session_meta |= parsed.get("type").and_then(Value::as_str) == Some("session_meta")
                || payload.get("type").and_then(Value::as_str) == Some("session_meta");
            if agent.is_none() {
                agent = codex_agent_metadata(payload);
            }

            if !found_session_id {
                if let Some(id) = payload.get("id").and_then(Value::as_str) {
                    session_id = id.to_string();
                    found_session_id = true;
                }
            }
            if cwd.is_empty() {
                cwd = payload
                    .get("cwd")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
            }
            if preview.is_empty() {
                match payload.get("type").and_then(Value::as_str) {
                    Some("user_message") | Some("agent_message") => {
                        preview = payload
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .chars()
                            .take(120)
                            .collect();
                    }
                    Some("message") => {
                        if let Some(role) = codex_message_role(payload) {
                            for part in codex_message_text_parts(payload) {
                                if should_include_response_message(role, &part.text) {
                                    preview = part.text.chars().take(120).collect();
                                    break;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }

            if !cwd.is_empty()
                && !preview.is_empty()
                && (saw_session_meta || agent.is_some() || parsed_lines >= 256)
            {
                break;
            }
        }

        SummaryData {
            session_id,
            cwd,
            preview,
            agent,
        }
    }

    fn cached_scan_summary(
        &self,
        path: &Path,
        session_key: &str,
        cache: Option<&SessionSummaryCache<'_>>,
    ) -> SummaryData {
        let Some(cache) = cache else {
            return self.scan_summary(path);
        };
        let Some(fingerprint) = SessionSummaryCache::fingerprint(path) else {
            return self.scan_summary(path);
        };

        if let Some(cached) = cache.get("codex", session_key, &fingerprint) {
            return SummaryData {
                session_id: cached.session_id,
                cwd: cached.cwd,
                preview: cached.preview,
                agent: self.scan_agent_metadata(path),
            };
        }

        let summary = self.scan_summary(path);
        let cached = CachedSessionSummary {
            session_id: summary.session_id.clone(),
            title: String::new(),
            preview: summary.preview.clone(),
            updated_at: modified_nanos(path).to_string(),
            cwd: summary.cwd.clone(),
        };
        let _ = cache.upsert("codex", session_key, &fingerprint, &cached);
        summary
    }

    fn scan_agent_metadata(&self, path: &Path) -> Option<CodexAgentMetadata> {
        let file = File::open(path).ok()?;
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(parsed) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            let payload = parsed.get("payload");
            if let Some(metadata) = payload.and_then(codex_agent_metadata) {
                return Some(metadata);
            }
            if parsed.get("type").and_then(Value::as_str) == Some("session_meta")
                || payload
                    .and_then(|value| value.get("type"))
                    .and_then(Value::as_str)
                    == Some("session_meta")
            {
                return None;
            }
        }
        None
    }

    fn thread_id(&self, lines: &[Value], path: &Path) -> String {
        for line in lines {
            if let Some(id) = line
                .get("payload")
                .and_then(|payload| payload.get("id"))
                .and_then(Value::as_str)
            {
                return id.to_string();
            }
        }

        path.file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("unknown")
            .to_string()
    }

    fn cwd(&self, lines: &[Value]) -> String {
        for line in lines {
            if let Some(cwd) = line
                .get("payload")
                .and_then(|payload| payload.get("cwd"))
                .and_then(Value::as_str)
            {
                return cwd.to_string();
            }
        }
        String::new()
    }

    fn blocks(&self, lines: &[Value], file_key: &str) -> Vec<TimelineBlock> {
        let mut blocks = Vec::new();
        let mut pending_tool_calls = Vec::new();
        let response_message_signatures = collect_response_message_signatures(lines);

        for (line_index, line) in lines.iter().enumerate() {
            let Some(payload) = line.get("payload") else {
                continue;
            };
            let block_start = blocks.len();
            let timestamp = line
                .get("timestamp")
                .or_else(|| payload.get("timestamp"))
                .and_then(Value::as_str)
                .map(ToString::to_string);

            match payload
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
            {
                "task_complete" if line.get("type").and_then(Value::as_str) == Some("event_msg") => {
                    if let Some(content) = codex_task_error_message(payload) {
                        let block = TimelineBlock {
                            id: format!("{line_index}:task_error"),
                            role: "assistant".to_string(),
                            content: content.to_string(),
                            editable: true,
                            edit_target: format!("{file_key}::{line_index}::task_error"),
                            source_meta: json!({
                                "lineIndex": line_index,
                                "payloadType": "task_complete",
                                "errorCode": payload.get("error")
                                    .and_then(|error| error.get("codex_error_info")),
                            }),
                            tool_calls: Vec::new(),
                        };
                        push_codex_block(&mut blocks, &mut pending_tool_calls, block);
                    }
                }
                "message" => {
                    if let Some(role) = codex_message_role(payload) {
                        for part in codex_message_text_parts(payload) {
                            if !should_include_response_message(role, &part.text) {
                                continue;
                            }

                            let block = TimelineBlock {
                                id: format!("{line_index}:{}:{role}", part.content_index),
                                role: role.to_string(),
                                content: part.text,
                                editable: true,
                                edit_target: format!(
                                    "{file_key}::{line_index}::{}::{}",
                                    part.content_index, part.field_name
                                ),
                                source_meta: json!({
                                    "lineIndex": line_index,
                                    "contentIndex": part.content_index,
                                    "payloadType": "message",
                                    "contentType": part.content_type,
                                }),
                                tool_calls: Vec::new(),
                            };
                            push_codex_block(&mut blocks, &mut pending_tool_calls, block);
                        }
                    }
                }
                "reasoning" => {
                    if let Some(block) = codex_reasoning_to_block(payload, line_index) {
                        blocks.push(block);
                    }
                }
                "user_message" => {
                    let content = payload
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if response_message_signatures.contains(&("user".to_string(), content.clone()))
                    {
                        continue;
                    }

                    let block = TimelineBlock {
                        id: format!("{line_index}:user"),
                        role: "user".to_string(),
                        content,
                        editable: true,
                        edit_target: format!("{file_key}::{line_index}"),
                        source_meta: json!({ "lineIndex": line_index }),
                        tool_calls: Vec::new(),
                    };
                    push_codex_block(&mut blocks, &mut pending_tool_calls, block);
                }
                "agent_message" => {
                    let content = payload
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if response_message_signatures
                        .contains(&("assistant".to_string(), content.clone()))
                    {
                        continue;
                    }

                    let block = TimelineBlock {
                        id: format!("{line_index}:assistant"),
                        role: "assistant".to_string(),
                        content,
                        editable: true,
                        edit_target: format!("{file_key}::{line_index}"),
                        source_meta: json!({ "lineIndex": line_index }),
                        tool_calls: Vec::new(),
                    };
                    push_codex_block(&mut blocks, &mut pending_tool_calls, block);
                }
                "function_call" => {
                    let tool_call = codex_function_call_to_block(payload, line_index);
                    push_codex_tool_call(&mut pending_tool_calls, tool_call);
                }
                "function_call_output" => {
                    let tool_call = codex_function_output_to_block(payload, line_index);
                    push_codex_tool_call(&mut pending_tool_calls, tool_call);
                }
                _ => {}
            }

            if let Some(timestamp) = &timestamp {
                for block in &mut blocks[block_start..] {
                    if let Some(meta) = block.source_meta.as_object_mut() {
                        meta.insert("createdAt".to_string(), json!(timestamp));
                    }
                }
            }
        }

        if !pending_tool_calls.is_empty() {
            if let Some(last_assistant) = blocks
                .iter_mut()
                .rev()
                .find(|block| block.role == "assistant")
            {
                last_assistant.tool_calls.append(&mut pending_tool_calls);
            }
        }

        blocks
    }

    fn includes_project_cwd(&self, cwd: &str) -> bool {
        let Some(project_root) = &self.project_root else {
            return true;
        };
        path_is_within_root(cwd, project_root)
    }

    fn searchable_content_entries(&self, session_key: &str) -> Vec<SessionContentEntry> {
        let mut entries = Vec::new();
        let mut indexed_bytes = 0usize;
        let mut msg_index = 0usize;

        let _ = visit_bounded_jsonl_lines(
            Path::new(session_key),
            MAX_INDEX_SOURCE_LINE_BYTES,
            |raw_line| {
                let Ok(line) = serde_json::from_slice::<Value>(raw_line) else {
                    return true;
                };
                let Some(payload) = line.get("payload") else {
                    return true;
                };
                let msg_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
                if msg_type == "task_complete"
                    && line.get("type").and_then(Value::as_str) == Some("event_msg")
                {
                    if let Some(message) = codex_task_error_message(payload) {
                        let entry = SessionContentEntry::joined_text(
                            msg_index,
                            "assistant",
                            vec![message.to_string()],
                        );
                        msg_index += 1;
                        return push_bounded_index_entry(&mut entries, &mut indexed_bytes, entry);
                    }
                    return true;
                }
                let role = match msg_type {
                    "message" => match codex_message_role(payload) {
                        Some(role) => role,
                        None => return true,
                    },
                    "reasoning" => "thinking",
                    "user_message" => "user",
                    "agent_message" => "assistant",
                    "function_call"
                    | "function_call_output"
                    | "custom_tool_call"
                    | "custom_tool_call_output"
                    | "web_search_call"
                    | "web_search_end"
                    | "patch_apply_end" => "assistant",
                    _ => return true,
                };
                let mut texts = Vec::new();

                if msg_type == "message" {
                    for part in codex_message_text_parts(payload) {
                        if should_include_response_message(role, &part.text) {
                            texts.push(part.text);
                        }
                    }
                } else if msg_type == "reasoning" {
                    if let Some(text) = codex_reasoning_text(payload) {
                        texts.push(text);
                    }
                } else if let Some(text) = payload.get("message").and_then(Value::as_str) {
                    texts.push(text.to_string());
                }
                if let Some(name) = payload.get("name").and_then(Value::as_str) {
                    texts.push(name.to_string());
                }
                if let Some(output) = payload.get("output").and_then(Value::as_str) {
                    texts.push(output.to_string());
                }
                if let Some(stdout) = payload.get("stdout").and_then(Value::as_str) {
                    texts.push(stdout.to_string());
                }
                if let Some(stderr) = payload.get("stderr").and_then(Value::as_str) {
                    texts.push(stderr.to_string());
                }
                if let Some(args) = payload.get("arguments") {
                    texts.push(args.to_string());
                }
                if let Some(input) = payload.get("input") {
                    texts.push(input.to_string());
                }

                let entry = SessionContentEntry::joined_text(msg_index, role, texts);
                msg_index += 1;
                push_bounded_index_entry(&mut entries, &mut indexed_bytes, entry)
            },
        );

        entries
    }
}

fn push_codex_block(
    blocks: &mut Vec<TimelineBlock>,
    pending_tool_calls: &mut Vec<ToolCallBlock>,
    mut block: TimelineBlock,
) {
    if block.role == "assistant" {
        block.tool_calls.append(pending_tool_calls);
    } else if !pending_tool_calls.is_empty() {
        if let Some(last_assistant) = blocks
            .iter_mut()
            .rev()
            .find(|block| block.role == "assistant")
        {
            last_assistant.tool_calls.append(pending_tool_calls);
        }
    }
    blocks.push(block);
}

fn push_codex_tool_call(pending_tool_calls: &mut Vec<ToolCallBlock>, tool_call: ToolCallBlock) {
    if let Some(existing) = pending_tool_calls
        .iter_mut()
        .find(|pending| pending.id == tool_call.id)
    {
        merge_codex_tool_call(existing, tool_call);
    } else {
        pending_tool_calls.push(tool_call);
    }
}

fn merge_codex_tool_call(existing: &mut ToolCallBlock, incoming: ToolCallBlock) {
    if existing.name == "function_call_output" && incoming.name != "function_call_output" {
        existing.name = incoming.name;
    }

    if existing.kind == "function_call_output" && incoming.kind != "function_call_output" {
        existing.kind = incoming.kind;
    }

    if incoming.input.is_some() {
        existing.input = incoming.input;
    }
    if incoming.output.is_some() {
        existing.output = incoming.output;
    }
    if incoming.error.is_some() {
        existing.error = incoming.error;
    }
    if incoming.started_at.is_some() {
        existing.started_at = incoming.started_at;
    }
    if incoming.ended_at.is_some() {
        existing.ended_at = incoming.ended_at;
    }

    existing.status = if existing.error.is_some() {
        "error".to_string()
    } else if existing.output.is_some() {
        "completed".to_string()
    } else {
        "requested".to_string()
    };

    existing.source_meta =
        merge_codex_tool_source_meta(&existing.source_meta, &incoming.source_meta);
}

fn merge_codex_tool_source_meta(left: &Value, right: &Value) -> Value {
    let mut line_indices = Vec::new();
    for value in [left, right] {
        if let Some(line_index) = value.get("lineIndex").and_then(Value::as_u64) {
            line_indices.push(line_index);
        }
        if let Some(items) = value.get("lineIndices").and_then(Value::as_array) {
            for item in items {
                if let Some(line_index) = item.as_u64() {
                    line_indices.push(line_index);
                }
            }
        }
    }
    line_indices.sort_unstable();
    line_indices.dedup();

    json!({
        "payloadType": "function_call",
        "lineIndices": line_indices,
    })
}

fn codex_function_call_to_block(payload: &Value, line_index: usize) -> ToolCallBlock {
    ToolCallBlock {
        id: payload
            .get("call_id")
            .or_else(|| payload.get("id"))
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("{line_index}:function_call")),
        name: payload
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("function_call")
            .to_string(),
        kind: "function_call".to_string(),
        status: "requested".to_string(),
        input: payload
            .get("arguments")
            .or_else(|| payload.get("input"))
            .and_then(|value| tool_text_from_value(value, 8192)),
        output: None,
        error: None,
        started_at: payload
            .get("timestamp")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        ended_at: None,
        source_meta: json!({
            "lineIndex": line_index,
            "payloadType": "function_call",
        }),
    }
}

fn codex_function_output_to_block(payload: &Value, line_index: usize) -> ToolCallBlock {
    ToolCallBlock {
        id: payload
            .get("call_id")
            .or_else(|| payload.get("id"))
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("{line_index}:function_call_output")),
        name: payload
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("function_call_output")
            .to_string(),
        kind: "function_call_output".to_string(),
        status: "completed".to_string(),
        input: None,
        output: payload.get("output").and_then(|value| {
            value
                .as_str()
                .and_then(|text| tool_text_from_str(text, 32768))
                .or_else(|| tool_text_from_value(value, 32768))
        }),
        error: payload
            .get("error")
            .and_then(|value| tool_text_from_value(value, 8192)),
        started_at: None,
        ended_at: payload
            .get("timestamp")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        source_meta: json!({
            "lineIndex": line_index,
            "payloadType": "function_call_output",
        }),
    }
}

impl PlatformAdapter for CodexPlatform {
    fn list_sessions(
        &self,
        alias_map: &HashMap<String, String>,
        limit: Option<usize>,
        offset: usize,
    ) -> SessionListResult {
        if !self.sessions_root.exists() {
            return SessionListResult {
                total: 0,
                items: Vec::new(),
            };
        }

        let mut entries = Vec::new();
        collect_jsonl_recursive(&self.sessions_root, &mut entries);
        entries.sort_by(|a, b| modified_nanos(b).cmp(&modified_nanos(a)));

        if self.project_root.is_none() {
            let total = entries.len();
            let page = if offset < total {
                let end = limit.map(|l| (offset + l).min(total)).unwrap_or(total);
                &entries[offset..end]
            } else {
                &[]
            };

            let items = page
                .iter()
                .map(|path| self.session_item(path, alias_map, None))
                .collect();
            return SessionListResult { total, items };
        }

        let items: Vec<SessionListItem> = entries
            .iter()
            .filter_map(|path| {
                let item = self.session_item(path, alias_map, None);
                if self.includes_project_cwd(&item.cwd) {
                    Some(item)
                } else {
                    None
                }
            })
            .collect();
        let total = items.len();
        let items = items
            .into_iter()
            .skip(offset)
            .take(limit.unwrap_or(usize::MAX))
            .collect();

        SessionListResult { total, items }
    }

    fn list_sessions_with_cache(
        &self,
        alias_map: &HashMap<String, String>,
        limit: Option<usize>,
        offset: usize,
        cache: Option<&SessionSummaryCache<'_>>,
    ) -> SessionListResult {
        if !self.sessions_root.exists() {
            return SessionListResult {
                total: 0,
                items: Vec::new(),
            };
        }

        let mut entries = Vec::new();
        collect_jsonl_recursive(&self.sessions_root, &mut entries);
        entries.sort_by(|a, b| modified_nanos(b).cmp(&modified_nanos(a)));

        if self.project_root.is_none() {
            let total = entries.len();
            let page = if offset < total {
                let end = limit.map(|l| (offset + l).min(total)).unwrap_or(total);
                &entries[offset..end]
            } else {
                &[]
            };

            let items = page
                .iter()
                .map(|path| self.session_item(path, alias_map, cache))
                .collect();
            return SessionListResult { total, items };
        }

        let items: Vec<SessionListItem> = entries
            .iter()
            .filter_map(|path| {
                let item = self.session_item(path, alias_map, cache);
                if self.includes_project_cwd(&item.cwd) {
                    Some(item)
                } else {
                    None
                }
            })
            .collect();
        let total = items.len();
        let items = items
            .into_iter()
            .skip(offset)
            .take(limit.unwrap_or(usize::MAX))
            .collect();

        SessionListResult { total, items }
    }

    fn list_session_keys(&self) -> Option<Vec<SessionKey>> {
        if !self.sessions_root.exists() || self.project_root.is_some() {
            return None;
        }

        let mut entries = Vec::new();
        collect_jsonl_recursive(&self.sessions_root, &mut entries);
        Some(
            entries
                .into_iter()
                .map(|path| {
                    let summary = self.scan_summary(&path);
                    SessionKey {
                        key: encode_path_key(&path),
                        sort_key: modified_nanos(&path) as i128,
                        session_id: summary.session_id,
                        parent_session_id: summary
                            .agent
                            .and_then(|metadata| metadata.parent_session_id),
                    }
                })
                .collect(),
        )
    }

    fn session_list_item(
        &self,
        session_key: &str,
        alias_map: &HashMap<String, String>,
        cache: Option<&SessionSummaryCache<'_>>,
    ) -> Option<SessionListItem> {
        let path = Path::new(session_key);
        if !path.exists() {
            return None;
        }
        let item = self.session_item(path, alias_map, cache);
        if self.project_root.is_some() && !self.includes_project_cwd(&item.cwd) {
            return None;
        }
        Some(item)
    }

    fn get_session_detail(
        &self,
        session_key: &str,
        alias_map: &HashMap<String, String>,
    ) -> Result<SessionDetail, String> {
        let path = Path::new(session_key);
        let lines = self.read_jsonl(path);
        let thread_id = self.thread_id(&lines, path);
        let alias = alias_map.get(session_key).cloned().unwrap_or_default();

        Ok(SessionDetail {
            platform: "codex".to_string(),
            session_key: session_key.to_string(),
            session_id: thread_id.clone(),
            title: if alias.is_empty() {
                thread_id.clone()
            } else {
                alias.clone()
            },
            alias_title: alias,
            cwd: self.cwd(&lines),
            commands: build_commands("codex", &thread_id),
            blocks: self.blocks(&lines, session_key),
        })
    }

    fn update_message(&self, edit_target: &str, new_content: &str) -> Result<String, String> {
        let parts: Vec<&str> = edit_target.split("::").collect();
        if parts.len() != 2 && parts.len() != 4 && (parts.len() != 3 || parts[2] != "task_error") {
            return Err(format!("Invalid edit target: {edit_target}"));
        }

        let path = Path::new(parts[0]);
        let line_index: usize = parts[1]
            .parse()
            .map_err(|error| format!("Invalid line index: {error}"))?;
        let original_raw = fs::read(path).map_err(|error| format!("Read error: {error}"))?;
        let mut rows = String::from_utf8_lossy(&original_raw)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect::<Vec<_>>();
        let thread_id = self.thread_id(&rows, path);

        let (old_content, role, message_id, error_turn_id) = if parts.len() == 3 {
            let Some(row) = rows.get_mut(line_index) else {
                return Err("Line index out of range".to_string());
            };
            if row.get("type").and_then(Value::as_str) != Some("event_msg") {
                return Err("Edit target does not point to a Codex turn error".to_string());
            }
            let Some(payload) = row.get_mut("payload") else {
                return Err("Missing payload".to_string());
            };
            if payload.get("type").and_then(Value::as_str) != Some("task_complete") {
                return Err("Edit target does not point to a Codex turn error".to_string());
            }
            let turn_id = payload
                .get("turn_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or("Missing Codex turn ID")?
                .to_string();
            let old_content = match payload.get("error") {
                Some(Value::Null) => String::new(),
                Some(error) => error.get("message").and_then(Value::as_str)
                    .ok_or("Missing Codex turn error message")?.to_string(),
                None => return Err("Missing Codex turn error".to_string()),
            };
            if new_content.is_empty() {
                payload["error"] = Value::Null;
            } else if payload["error"].is_null() {
                payload["error"] = json!({ "message": new_content });
            } else {
                payload["error"]["message"] = Value::String(new_content.to_string());
            }
            (old_content, "task_error".to_string(), String::new(), Some(turn_id))
        } else if parts.len() == 2 {
            let Some(row) = rows.get_mut(line_index) else {
                return Err("Line index out of range".to_string());
            };
            let Some(payload) = row.get_mut("payload") else {
                return Err("Missing payload".to_string());
            };
            let role = match payload.get("type").and_then(Value::as_str) {
                Some("user_message") => "user",
                Some("agent_message") => "assistant",
                _ => "",
            }
            .to_string();

            let old_content = payload
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            payload["message"] = Value::String(new_content.to_string());
            let message_id = payload
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            (old_content, role, message_id, None)
        } else {
            let content_index: usize = parts[2]
                .parse()
                .map_err(|error| format!("Invalid content index: {error}"))?;
            let field_name = parts[3];

            let Some(row) = rows.get_mut(line_index) else {
                return Err("Line index out of range".to_string());
            };
            let Some(payload) = row.get_mut("payload") else {
                return Err("Missing payload".to_string());
            };

            if payload.get("type").and_then(Value::as_str) != Some("message") {
                return Err("Edit target does not point to a Codex message".to_string());
            }

            let role = payload
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let message_id = payload
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();

            let old_content = if field_name == "content" {
                let Some(content) = payload.get_mut("content") else {
                    return Err("Missing message content".to_string());
                };
                let old = content.as_str().unwrap_or_default().to_string();
                *content = Value::String(new_content.to_string());
                old
            } else {
                let Some(items) = payload.get_mut("content").and_then(Value::as_array_mut) else {
                    return Err("Message content is not an array".to_string());
                };
                let Some(item) = items.get_mut(content_index) else {
                    return Err("Content index out of range".to_string());
                };
                let old = item
                    .get(field_name)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                item[field_name] = Value::String(new_content.to_string());
                old
            };

            (old_content, role, message_id, None)
        };

        let history_item_id = if role == "user" {
            find_user_history_item_id(&rows, line_index, &old_content)
        } else if role == "assistant" && !message_id.is_empty() {
            Some(message_id.clone())
        } else {
            None
        };

        if error_turn_id.is_none() {
            sync_mirrored_message(&mut rows, line_index, &role, &old_content, new_content);
        }

        let mut in_place_patch = None;
        let updated_raw = if error_turn_id.is_some() {
            let mut updated = serde_json::to_string(&rows[line_index])
                .map_err(|error| format!("Serialize error: {error}"))?;
            let raw_lines = original_raw.split_inclusive(|byte| *byte == b'\n').collect::<Vec<_>>();
            let original_line = raw_lines.get(line_index).ok_or("Codex rollout line disappeared")?;
            let suffix_len = if original_line.ends_with(b"\r\n") { 2 } else if original_line.ends_with(b"\n") { 1 } else { 0 };
            if updated.len() <= original_line.len() - suffix_len {
                updated.push_str(&" ".repeat(original_line.len() - suffix_len - updated.len()));
                in_place_patch = Some((
                    raw_lines[..line_index].iter().map(|line| line.len()).sum::<usize>() as u64,
                    original_line.to_vec(),
                ));
            }
            rewrite_jsonl_row(&original_raw, line_index, rows.len(), &updated)?
        } else {
            let serialized = rows
                .iter()
                .map(serde_json::to_string)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| format!("Serialize error: {error}"))?;
            format!("{}\n", serialized.join("\n")).into_bytes()
        };
        if error_turn_id.is_some() && fs::read(path).map_err(|error| format!("Read error: {error}"))? != original_raw {
            return Err("Codex rollout changed during edit".to_string());
        }
        if error_turn_id.is_some() && in_place_patch.is_none() && codex_rollout_has_writer(path) {
            return Err("Codex is writing this session; close it before a length-changing error edit".to_string());
        }
        if let Some((offset, original_line)) = &in_place_patch {
            let updated_line = &updated_raw[*offset as usize..*offset as usize + original_line.len()];
            if let Err(error) = write_jsonl_line(path, *offset, updated_line) {
                let _ = rollback_jsonl(path, &original_raw, &in_place_patch);
                return Err(format!("Write error: {error}"));
            }
        } else {
            fs::write(path, &updated_raw)
                .map_err(|error| format!("Write error: {error}"))?;
        }

        if let Some(history_item_id) = history_item_id {
            match sync_history_projection(
                &self.codex_home,
                &thread_id,
                &history_item_id,
                &role,
                &old_content,
                new_content,
            ) {
                Ok(updated) => {
                    eprintln!(
                        "[codex] history projection sync thread={thread_id} item={history_item_id} databases_updated={updated}"
                    );
                }
                Err(error) => {
                    let restore_result = rollback_jsonl(path, &original_raw, &in_place_patch);
                    let restore_note = restore_result
                        .err()
                        .map(|restore_error| format!("; JSONL rollback also failed: {restore_error}"))
                        .unwrap_or_default();
                    return Err(format!(
                        "Codex history projection sync failed; JSONL edit was rolled back: {error}{restore_note}"
                    ));
                }
            }
        }

        if let Some(turn_id) = error_turn_id {
            if let Err(error) = sync_turn_error_projection(
                &self.codex_home,
                &thread_id,
                &turn_id,
                &old_content,
                new_content,
                &jsonl_line_offsets(&updated_raw),
            ) {
                let restore_note = rollback_jsonl(path, &original_raw, &in_place_patch)
                    .err()
                    .map(|restore_error| format!("; JSONL rollback also failed: {restore_error}"))
                    .unwrap_or_default();
                return Err(format!(
                    "Codex turn error sync failed; JSONL edit was rolled back: {error}{restore_note}"
                ));
            }
        }

        Ok(old_content)
    }

    fn matches_query(&self, session_key: &str, query: &str) -> bool {
        !self.content_search(session_key, query).is_empty()
    }

    fn warm_content_index(
        &self,
        session_key: &str,
        index: Option<&SessionContentIndex<'_>>,
    ) -> bool {
        let Some(index) = index else {
            return false;
        };
        let path = Path::new(session_key);
        let Some(fingerprint) = SessionSummaryCache::fingerprint(path) else {
            return false;
        };
        if index.is_current("codex", session_key, &fingerprint) {
            return true;
        }
        let entries = self.searchable_content_entries(session_key);
        index
            .replace("codex", session_key, &fingerprint, &entries)
            .is_ok()
    }

    fn has_current_content_index(
        &self,
        session_key: &str,
        index: Option<&SessionContentIndex<'_>>,
    ) -> bool {
        let (Some(index), Some(fingerprint)) = (
            index,
            SessionSummaryCache::fingerprint(Path::new(session_key)),
        ) else {
            return false;
        };
        index.is_current("codex", session_key, &fingerprint)
    }

    fn content_search(&self, session_key: &str, query: &str) -> Vec<ContentMatch> {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return vec![];
        }

        content_entries_to_matches(self.searchable_content_entries(session_key), &needle)
    }

    fn content_search_with_index(
        &self,
        session_key: &str,
        query: &str,
        index: Option<&SessionContentIndex<'_>>,
    ) -> Vec<ContentMatch> {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return vec![];
        }

        let Some(index) = index else {
            return self.content_search(session_key, &needle);
        };
        let path = Path::new(session_key);
        let Some(fingerprint) = SessionSummaryCache::fingerprint(path) else {
            return self.content_search(session_key, &needle);
        };

        if let Some(entries) = index.get_matches("codex", session_key, &fingerprint, &needle) {
            return content_entries_to_matches(entries, &needle);
        }

        let entries = self.searchable_content_entries(session_key);
        let matches = content_entries_to_matches(entries.clone(), &needle);
        let _ = index.replace("codex", session_key, &fingerprint, &entries);
        matches
    }
}

fn collect_response_message_signatures(lines: &[Value]) -> HashSet<(String, String)> {
    let mut signatures = HashSet::new();

    for line in lines {
        let Some(payload) = line.get("payload") else {
            continue;
        };
        if payload.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let Some(role) = codex_message_role(payload) else {
            continue;
        };
        for part in codex_message_text_parts(payload) {
            if should_include_response_message(role, &part.text) {
                signatures.insert((role.to_string(), part.text));
            }
        }
    }

    signatures
}

fn codex_message_role(payload: &Value) -> Option<&str> {
    match payload.get("role").and_then(Value::as_str) {
        Some("user") => Some("user"),
        Some("assistant") => Some("assistant"),
        _ => None,
    }
}

fn codex_message_text_parts(payload: &Value) -> Vec<CodexTextPart> {
    let Some(content) = payload.get("content") else {
        return Vec::new();
    };

    if let Some(text) = content.as_str() {
        return vec![CodexTextPart {
            content_index: 0,
            field_name: "content".to_string(),
            content_type: "text".to_string(),
            text: text.to_string(),
        }];
    }

    let Some(items) = content.as_array() else {
        return Vec::new();
    };

    items
        .iter()
        .enumerate()
        .filter_map(|(content_index, item)| {
            let text = item.get("text").and_then(Value::as_str)?;
            Some(CodexTextPart {
                content_index,
                field_name: "text".to_string(),
                content_type: item
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("text")
                    .to_string(),
                text: text.to_string(),
            })
        })
        .collect()
}

fn codex_task_error_message(payload: &Value) -> Option<&str> {
    payload
        .get("error")?
        .get("message")?
        .as_str()
        .filter(|message| !message.trim().is_empty())
}

fn should_include_response_message(role: &str, text: &str) -> bool {
    role != "user" || !is_internal_codex_user_message(text)
}

fn is_internal_codex_user_message(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("<environment_context>")
        || trimmed.starts_with(
            "The following is the Codex agent history whose request action you are assessing.",
        )
        || trimmed.starts_with(
            "The following is the Codex agent history added since your last approval assessment.",
        )
}

fn codex_reasoning_to_block(payload: &Value, line_index: usize) -> Option<TimelineBlock> {
    let content = codex_reasoning_text(payload)?;

    Some(TimelineBlock {
        id: format!("{line_index}:reasoning"),
        role: "thinking".to_string(),
        content,
        editable: false,
        edit_target: String::new(),
        source_meta: json!({
            "lineIndex": line_index,
            "payloadType": "reasoning",
            "encrypted": payload.get("encrypted_content").and_then(Value::as_str).is_some(),
        }),
        tool_calls: Vec::new(),
    })
}

fn codex_reasoning_text(payload: &Value) -> Option<String> {
    let mut pieces = Vec::new();

    if let Some(content) = payload.get("content") {
        collect_codex_text(content, &mut pieces);
    }

    if let Some(summary) = payload.get("summary") {
        collect_codex_text(summary, &mut pieces);
    }

    let text = pieces
        .into_iter()
        .map(|piece| piece.trim().to_string())
        .filter(|piece| !piece.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");

    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn collect_codex_text(value: &Value, out: &mut Vec<String>) {
    if let Some(text) = value.as_str() {
        out.push(text.to_string());
        return;
    }

    if let Some(items) = value.as_array() {
        for item in items {
            collect_codex_text(item, out);
        }
        return;
    }

    if let Some(object) = value.as_object() {
        for key in ["text", "summary", "content"] {
            if let Some(nested) = object.get(key) {
                collect_codex_text(nested, out);
            }
        }
    }
}

fn sync_mirrored_message(
    rows: &mut [Value],
    changed_line_index: usize,
    role: &str,
    old_content: &str,
    new_content: &str,
) {
    if role.is_empty() || old_content.is_empty() {
        return;
    }

    for (line_index, row) in rows.iter_mut().enumerate() {
        if line_index == changed_line_index {
            continue;
        }
        if line_index.abs_diff(changed_line_index) > 2 {
            continue;
        }

        if row.get("type").and_then(Value::as_str) == Some("event_msg") {
            let expected_type = match role {
                "assistant" => "AgentMessage",
                "user" => "UserMessage",
                _ => continue,
            };
            let Some(item) = row
                .get_mut("payload")
                .and_then(|event| event.get_mut("item"))
            else {
                continue;
            };
            if item.get("type").and_then(Value::as_str) != Some(expected_type) {
                continue;
            }
            if let Some(content) = item.get_mut("content") {
                update_mirrored_response_content(content, old_content, new_content);
            }
            continue;
        }

        let Some(payload) = row.get_mut("payload") else {
            continue;
        };
        match payload.get("type").and_then(Value::as_str) {
            Some("user_message") if role == "user" => {
                if payload.get("message").and_then(Value::as_str) == Some(old_content) {
                    payload["message"] = Value::String(new_content.to_string());
                }
            }
            Some("agent_message") if role == "assistant" => {
                if payload.get("message").and_then(Value::as_str) == Some(old_content) {
                    payload["message"] = Value::String(new_content.to_string());
                }
            }
            Some("message") => {
                if payload.get("role").and_then(Value::as_str) != Some(role) {
                    continue;
                }
                if let Some(content) = payload.get_mut("content") {
                    update_mirrored_response_content(content, old_content, new_content);
                }
            }
            _ => {}
        }
    }
}

fn update_mirrored_response_content(content: &mut Value, old_content: &str, new_content: &str) {
    if content.as_str() == Some(old_content) {
        *content = Value::String(new_content.to_string());
        return;
    }

    let Some(items) = content.as_array_mut() else {
        return;
    };

    for item in items {
        if item.get("text").and_then(Value::as_str) == Some(old_content) {
            item["text"] = Value::String(new_content.to_string());
            return;
        }
    }
}

fn find_user_history_item_id(rows: &[Value], message_line_index: usize, old_content: &str) -> Option<String> {
    let payload = rows.get(message_line_index)?.get("payload")?;
    let turn_id = payload
        .get("internal_chat_message_metadata_passthrough")?
        .get("turn_id")?
        .as_str()?;
    let mut matching_ids = rows.iter().filter_map(|row| {
        let event = row.get("payload")?;
        if row.get("type").and_then(Value::as_str) != Some("event_msg")
            || event.get("turn_id").and_then(Value::as_str) != Some(turn_id)
        {
            return None;
        }
        let item = event.get("item")?;
        if item.get("type").and_then(Value::as_str) != Some("UserMessage")
            || !content_contains_text(item.get("content")?, old_content)
        {
            return None;
        }
        item.get("id").and_then(Value::as_str).map(str::to_string)
    });
    let id = matching_ids.next()?;
    matching_ids.next().is_none().then_some(id)
}

fn content_contains_text(content: &Value, expected: &str) -> bool {
    match content {
        Value::String(text) => text == expected,
        Value::Array(parts) => parts
            .iter()
            .any(|part| part.get("text").and_then(Value::as_str) == Some(expected)),
        _ => false,
    }
}

fn write_jsonl_line(path: &Path, offset: u64, line: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(line)?;
    file.sync_data()
}

fn rollback_jsonl(path: &Path, original: &[u8], patch: &Option<(u64, Vec<u8>)>) -> std::io::Result<()> {
    if let Some((offset, line)) = patch {
        write_jsonl_line(path, *offset, line)
    } else {
        fs::write(path, original)
    }
}

#[cfg(target_os = "linux")]
fn codex_rollout_has_writer(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    let Ok(target) = fs::metadata(path) else { return true };
    let Ok(processes) = fs::read_dir("/proc") else { return true };
    for process in processes.flatten() {
        if !process.file_name().to_string_lossy().bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let Ok(descriptors) = fs::read_dir(process.path().join("fd")) else { continue };
        for descriptor in descriptors.flatten() {
            let Ok(file) = fs::metadata(descriptor.path()) else { continue };
            if file.dev() != target.dev() || file.ino() != target.ino() {
                continue;
            }
            let Ok(info) = fs::read_to_string(process.path().join("fdinfo").join(descriptor.file_name())) else { continue };
            let flags = info.lines().find_map(|line| line.strip_prefix("flags:")?.trim().split_whitespace().next())
                .and_then(|value| u32::from_str_radix(value, 8).ok());
            if flags.is_some_and(|flags| flags & 3 != 0) {
                return true;
            }
        }
    }
    false
}

#[cfg(not(target_os = "linux"))]
fn codex_rollout_has_writer(_path: &Path) -> bool {
    false
}

fn rewrite_jsonl_row(raw: &[u8], target_index: usize, expected_rows: usize, updated: &str) -> Result<Vec<u8>, String> {
    let lines = raw.split_inclusive(|byte| *byte == b'\n').collect::<Vec<_>>();
    if lines.len() != expected_rows || lines.iter().any(|line| {
        serde_json::from_slice::<Value>(line).is_err()
    }) {
        return Err("Codex rollout contains blank or invalid rows; edit aborted".to_string());
    }
    let mut result = Vec::with_capacity(raw.len() + updated.len());
    for (index, line) in lines.into_iter().enumerate() {
        if index == target_index {
            result.extend_from_slice(updated.as_bytes());
            if line.ends_with(b"\r\n") {
                result.extend_from_slice(b"\r\n");
            } else if line.ends_with(b"\n") {
                result.push(b'\n');
            }
        } else {
            result.extend_from_slice(line);
        }
    }
    Ok(result)
}

fn jsonl_line_offsets(raw: &[u8]) -> Vec<i64> {
    let mut offsets = vec![0];
    for line in raw.split_inclusive(|byte| *byte == b'\n') {
        offsets.push(offsets.last().copied().unwrap_or(0) + line.len() as i64);
    }
    offsets
}

fn history_database_paths(codex_home: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = match fs::read_dir(codex_home) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("cannot scan Codex home: {error}")),
    };
    let mut database_paths = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                return false;
            };
            let Some(version) = name
                .strip_prefix("thread_history_")
                .and_then(|name| name.strip_suffix(".sqlite"))
            else {
                return false;
            };
            !version.is_empty()
                && version.chars().all(|character| character.is_ascii_digit())
                && path.is_file()
        })
        .collect::<Vec<_>>();
    database_paths.sort();
    Ok(database_paths)
}

fn sync_turn_error_projection(
    codex_home: &Path,
    thread_id: &str,
    turn_id: &str,
    old_content: &str,
    new_content: &str,
    offsets: &[i64],
) -> Result<usize, String> {
    let mut matching_databases = Vec::new();
    for database_path in history_database_paths(codex_home)? {
        let connection = Connection::open_with_flags(&database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|error| format!("cannot open Codex history database '{}': {error}", database_path.display()))?;
        connection.busy_timeout(Duration::from_secs(2))
            .map_err(|error| format!("cannot configure Codex history database: {error}"))?;
        let table_exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'thread_turns')",
            [],
            |row| row.get(0),
        ).map_err(|error| format!("cannot inspect Codex history schema: {error}"))?;
        if !table_exists {
            continue;
        }
        let count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM thread_turns WHERE thread_id = ?1 AND turn_id = ?2",
            [thread_id, turn_id],
            |row| row.get(0),
        ).map_err(|error| format!("cannot read Codex turn error: {error}"))?;
        if count > 1 {
            return Err(format!("Codex turn '{turn_id}' is duplicated in '{}'", database_path.display()));
        }
        if count == 1 {
            matching_databases.push(database_path);
        }
    }
    if matching_databases.len() > 1 {
        return Err(format!("Codex turn '{turn_id}' appears in multiple history databases"));
    }
    let Some(database_path) = matching_databases.pop() else {
        return Ok(0);
    };

    let mut connection = Connection::open_with_flags(&database_path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|error| format!("cannot open Codex history database '{}': {error}", database_path.display()))?;
    connection.busy_timeout(Duration::from_secs(2))
        .map_err(|error| format!("cannot configure Codex history database: {error}"))?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| format!("cannot lock Codex history database: {error}"))?;
    let (error_json, status): (Option<String>, String) = transaction.query_row(
        "SELECT error_json, status FROM thread_turns WHERE thread_id = ?1 AND turn_id = ?2",
        [thread_id, turn_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional()
        .map_err(|error| format!("cannot read Codex turn error: {error}"))?
        .ok_or_else(|| format!("Codex turn '{turn_id}' disappeared during update"))?;
    let mut error = error_json.as_deref()
        .map(serde_json::from_str::<Value>)
        .transpose().map_err(|error| format!("invalid Codex turn error JSON: {error}"))?
        .unwrap_or_else(|| json!({ "message": "" }));
    let current = error.get("message").and_then(Value::as_str)
        .ok_or("Codex turn error has no message")?;
    if current != old_content && current != new_content {
        return Err(format!("Codex turn '{turn_id}' error no longer matches the edited text"));
    }
    let new_status = if new_content.is_empty() { "completed" } else { "failed" };
    let new_error_json = if new_content.is_empty() {
        None
    } else {
        error["message"] = Value::String(new_content.to_string());
        Some(error.to_string())
    };
    let changed = error_json != new_error_json || status != new_status;
    if changed {
        let updated = transaction.execute(
            "UPDATE thread_turns SET error_json = ?1, status = ?2 WHERE thread_id = ?3 AND turn_id = ?4",
            rusqlite::params![new_error_json, new_status, thread_id, turn_id],
        ).map_err(|error| format!("cannot update Codex turn error: {error}"))?;
        if updated != 1 {
            return Err(format!("expected to update one Codex turn, updated {updated}"));
        }
    }
    let mut turns = transaction.prepare(
        "SELECT turn_id, rollout_ordinal, rollout_end_ordinal FROM thread_turns WHERE thread_id = ?1"
    ).map_err(|error| format!("cannot inspect Codex rollout offsets: {error}"))?;
    let turn_offsets = turns.query_map([thread_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<usize>>(1)?, row.get::<_, Option<usize>>(2)?))
    }).map_err(|error| format!("cannot read Codex rollout offsets: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("cannot read Codex rollout offsets: {error}"))?;
    drop(turns);
    for (id, start, end) in turn_offsets {
        let (Some(start), Some(end)) = (start, end) else { continue };
        let (Some(&start_offset), Some(&end_offset)) = (offsets.get(start), offsets.get(end + 1)) else {
            return Err(format!("Codex rollout offsets out of range for turn '{id}'"));
        };
        transaction.execute(
            "UPDATE thread_turns SET rollout_byte_offset = ?1, rollout_end_byte_offset = ?2 WHERE thread_id = ?3 AND turn_id = ?4",
            rusqlite::params![start_offset, end_offset, thread_id, id],
        ).map_err(|error| format!("cannot update Codex rollout offsets: {error}"))?;
    }
    let next_ordinal: Option<usize> = transaction.query_row(
        "SELECT next_rollout_ordinal FROM thread_history_projection_state WHERE thread_id = ?1",
        [thread_id], |row| row.get(0),
    ).optional().map_err(|error| format!("cannot read Codex projection state: {error}"))?;
    if let Some(next_ordinal) = next_ordinal {
        let next_offset = offsets.get(next_ordinal)
            .ok_or("Codex projection ordinal exceeds rollout length")?;
        transaction.execute(
            "UPDATE thread_history_projection_state SET next_rollout_byte_offset = ?1 WHERE thread_id = ?2",
            rusqlite::params![next_offset, thread_id],
        ).map_err(|error| format!("cannot update Codex projection state: {error}"))?;
    }
    transaction.commit().map_err(|error| format!("cannot commit Codex turn error update: {error}"))?;
    Ok(usize::from(changed))
}

fn sync_history_projection(
    codex_home: &Path,
    thread_id: &str,
    item_id: &str,
    role: &str,
    old_content: &str,
    new_content: &str,
) -> Result<usize, String> {
    let database_paths = history_database_paths(codex_home)?;

    let mut matching_databases = Vec::new();
    for database_path in database_paths {
        let connection = Connection::open_with_flags(
            &database_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| {
            format!(
                "cannot open Codex history database '{}': {error}",
                database_path.display()
            )
        })?;
        connection
            .busy_timeout(Duration::from_secs(2))
            .map_err(|error| format!("cannot configure Codex history database: {error}"))?;

        let table_exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'thread_items')",
                [],
                |row| row.get(0),
            )
            .map_err(|error| format!("cannot inspect Codex history schema: {error}"))?;
        if !table_exists {
            continue;
        }

        let mut statement = connection
            .prepare("PRAGMA table_info(thread_items)")
            .map_err(|error| format!("cannot inspect Codex thread_items columns: {error}"))?;
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|error| format!("cannot inspect Codex thread_items columns: {error}"))?
            .collect::<rusqlite::Result<HashSet<_>>>()
            .map_err(|error| format!("cannot read Codex thread_items columns: {error}"))?;
        drop(statement);
        if !["thread_id", "item_id", "item_json"]
            .into_iter()
            .all(|column| columns.contains(column))
        {
            continue;
        }

        let matching_rows: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM thread_items WHERE thread_id = ?1 AND item_id = ?2",
                [thread_id, item_id],
                |row| row.get(0),
            )
            .map_err(|error| format!("cannot read Codex projected message: {error}"))?;
        if matching_rows > 1 {
            return Err(format!(
                "Codex item '{item_id}' is duplicated in history database '{}'",
                database_path.display()
            ));
        }
        if matching_rows == 1 {
            matching_databases.push(database_path);
        }
    }

    if matching_databases.len() > 1 {
        return Err(format!(
            "Codex thread '{thread_id}' appears in multiple history databases"
        ));
    }
    let Some(database_path) = matching_databases.pop() else {
        return Ok(0);
    };

    let mut connection = Connection::open_with_flags(
        &database_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| {
        format!(
            "cannot open Codex history database '{}': {error}",
            database_path.display()
        )
    })?;
    connection
        .busy_timeout(Duration::from_secs(2))
        .map_err(|error| format!("cannot configure Codex history database: {error}"))?;

    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| format!("cannot lock Codex history database: {error}"))?;
    let item_json: String = transaction
        .query_row(
            "SELECT item_json FROM thread_items WHERE thread_id = ?1 AND item_id = ?2 LIMIT 1",
            [thread_id, item_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| format!("cannot read Codex projected message: {error}"))?
        .ok_or_else(|| format!("Codex projected item '{item_id}' disappeared during update"))?;

    let mut item: Value = serde_json::from_str(&item_json)
        .map_err(|error| format!("invalid Codex projected message JSON: {error}"))?;
    let expected_type = match role {
        "assistant" => "agentMessage",
        "user" => "userMessage",
        _ => return Ok(0),
    };
    if item.get("type").and_then(Value::as_str) != Some(expected_type) {
        return Err(format!(
            "Codex projected item '{item_id}' has unexpected type"
        ));
    }

    let changed = match role {
        "assistant" => {
            let Some(text) = item.get("text").and_then(Value::as_str) else {
                return Err(format!("Codex projected assistant item '{item_id}' has no text"));
            };
            if text == new_content {
                false
            } else if text == old_content {
                item["text"] = Value::String(new_content.to_string());
                true
            } else {
                return Err(format!(
                    "Codex projected assistant item '{item_id}' no longer matches the edited text"
                ));
            }
        }
        "user" => {
            let Some(content) = item.get_mut("content").and_then(Value::as_array_mut) else {
                return Err(format!("Codex projected user item '{item_id}' has no content array"));
            };
            let mut found_old = false;
            let mut found_new = false;
            for part in content {
                if part.get("text").and_then(Value::as_str) == Some(old_content) {
                    part["text"] = Value::String(new_content.to_string());
                    found_old = true;
                    break;
                }
                found_new |= part.get("text").and_then(Value::as_str) == Some(new_content);
            }
            if found_old {
                true
            } else if found_new {
                false
            } else {
                return Err(format!(
                    "Codex projected user item '{item_id}' no longer matches the edited text"
                ));
            }
        }
        _ => return Ok(0),
    };

    if changed {
        let updated_rows = transaction
            .execute(
                "UPDATE thread_items SET item_json = ?1 WHERE thread_id = ?2 AND item_id = ?3",
                rusqlite::params![
                    serde_json::to_string(&item)
                        .map_err(|error| format!("cannot serialize projected message: {error}"))?,
                    thread_id,
                    item_id
                ],
            )
            .map_err(|error| format!("cannot update Codex projected message: {error}"))?;
        if updated_rows != 1 {
            return Err(format!(
                "expected to update one Codex projected item, updated {updated_rows}"
            ));
        }
    }
    transaction
        .commit()
        .map_err(|error| format!("cannot commit Codex history update: {error}"))?;

    Ok(if changed { 1 } else { 0 })
}

fn encode_path_key(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .replace('\\', "/")
}

fn modified_nanos(path: &Path) -> u128 {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .and_then(|time| {
            time.duration_since(SystemTime::UNIX_EPOCH)
                .map_err(std::io::Error::other)
        })
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

fn collect_jsonl_recursive(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl_recursive(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

impl CodexPlatform {
    fn session_item(
        &self,
        path: &Path,
        alias_map: &HashMap<String, String>,
        cache: Option<&SessionSummaryCache<'_>>,
    ) -> SessionListItem {
        let session_key = encode_path_key(path);
        let summary = self.cached_scan_summary(path, &session_key, cache);
        let alias = alias_map.get(&session_key).cloned().unwrap_or_default();

        SessionListItem {
            platform: "codex".to_string(),
            session_key,
            session_id: summary.session_id.clone(),
            display_title: if alias.is_empty() {
                summary.session_id
            } else {
                alias.clone()
            },
            alias_title: alias,
            preview: summary.preview,
            updated_at: modified_nanos(path).to_string(),
            cwd: summary.cwd,
            editable: true,
            content_matches: vec![],
            total_content_matches: 0,
            favorite: false,
            agent_group: summary.agent.map(|agent| SessionAgentGroup {
                parent_session_id: agent.parent_session_id,
                depth: agent.depth,
                nickname: agent.nickname,
                role: agent.role,
                path: agent.path,
                orphaned: false,
                children: Vec::new(),
            }),
        }
    }
}

fn codex_agent_metadata(payload: &Value) -> Option<CodexAgentMetadata> {
    let source_spawn = payload
        .get("source")
        .and_then(|source| source.get("subagent"))
        .and_then(|subagent| subagent.get("thread_spawn"));
    let is_subagent = source_spawn.is_some()
        || payload
            .get("parent_thread_id")
            .and_then(Value::as_str)
            .is_some()
        || payload.get("thread_source").and_then(Value::as_str) == Some("subagent");
    if !is_subagent {
        return None;
    }

    let text = |name: &str| {
        payload
            .get(name)
            .and_then(Value::as_str)
            .or_else(|| {
                source_spawn
                    .and_then(|spawn| spawn.get(name))
                    .and_then(Value::as_str)
            })
            .map(ToString::to_string)
    };
    let depth = source_spawn
        .and_then(|spawn| spawn.get("depth"))
        .and_then(Value::as_i64)
        .and_then(|value| i32::try_from(value).ok());

    Some(CodexAgentMetadata {
        parent_session_id: text("parent_thread_id"),
        depth,
        nickname: text("agent_nickname"),
        role: text("agent_role").or_else(|| text("agent_type")),
        path: text("agent_path"),
    })
}

fn path_is_within_root(path: &str, root: &Path) -> bool {
    let path = normalize_path_for_prefix(path);
    let root = normalize_path_for_prefix(&root.to_string_lossy());

    if path.is_empty() || root.is_empty() {
        return false;
    }

    path == root || path.starts_with(&format!("{root}/"))
}

fn normalize_path_for_prefix(path: &str) -> String {
    let mut value = path.trim().replace('\\', "/");
    while value.ends_with('/') {
        value.pop();
    }
    #[cfg(windows)]
    {
        value = value.to_lowercase();
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_complete_error_is_displayed_and_indexed_without_editing_the_rollout() {
        let root = std::env::temp_dir().join(format!(
            "memory-forge-codex-task-error-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir_all(&root).expect("create temp dir");
        let path = root.join("session.jsonl");
        let lines = vec![
            json!({ "type": "event_msg", "payload": { "type": "user_message", "message": "inspect the result" } }),
            json!({ "type": "event_msg", "payload": { "type": "task_complete", "error": null } }),
            json!({ "type": "event_msg", "timestamp": "2026-11-13T10:25:49.858Z", "payload": {
                "type": "task_complete", "last_agent_message": null,
                "error": { "message": "This content was flagged for possible cybersecurity risk.", "codex_error_info": "cyber_policy" }
            } }),
        ];
        fs::write(
            &path,
            lines.iter().map(Value::to_string).collect::<Vec<_>>().join("\n") + "\n",
        ).expect("write session");

        let platform = CodexPlatform::new(root.clone(), None);
        let detail = platform.get_session_detail(path.to_str().unwrap(), &HashMap::new())
            .expect("read detail");
        assert_eq!(detail.blocks.len(), 2);
        let error = &detail.blocks[1];
        assert_eq!(error.content, "This content was flagged for possible cybersecurity risk.");
        assert_eq!(error.role, "assistant");
        assert!(error.editable);
        assert_eq!(error.edit_target, format!("{}::2::task_error", path.display()));
        assert_eq!(error.source_meta["payloadType"], "task_complete");
        assert_eq!(error.source_meta["errorCode"], "cyber_policy");
        assert_eq!(error.source_meta["createdAt"], "2026-11-13T10:25:49.858Z");
        let matches = platform.content_search(path.to_str().unwrap(), "cybersecurity risk");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].role, "assistant");

        let conn = Connection::open_in_memory().expect("open sqlite");
        crate::database::init_tables(&conn).expect("init sqlite");
        let conn = std::sync::Mutex::new(conn);
        let index = SessionContentIndex::new(&conn);
        let key = path.to_str().unwrap();
        let fingerprint = SessionSummaryCache::fingerprint(&path).expect("file fingerprint");
        {
            let conn = conn.lock().expect("db lock");
            conn.execute(
                "INSERT INTO session_content_index_meta
                    (platform, session_key, file_size, modified_at, schema_version)
                 VALUES (?1, ?2, ?3, ?4, 2)",
                rusqlite::params!["codex", key, fingerprint.file_size, fingerprint.modified_at],
            ).expect("insert old index metadata");
        }
        assert!(!platform.has_current_content_index(key, Some(&index)));
        assert!(platform.warm_content_index(key, Some(&index)));
        assert!(platform.has_current_content_index(key, Some(&index)));
        assert_eq!(platform.content_search_with_index(key, "cybersecurity risk", Some(&index)).len(), 1);
        assert!(index.search_platform("codex", "cybersecurity risk", 5).unwrap().contains_key(key));
        drop(index);
        drop(conn);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn turn_error_edit_erase_and_restore_sync_jsonl_both_databases_and_search() {
        let root = std::env::temp_dir().join(format!(
            "memory-forge-codex-turn-edit-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let codex_home = root.join("codex-home");
        let session_dir = codex_home.join("sessions");
        fs::create_dir_all(&session_dir).expect("create sessions");
        let path = session_dir.join("rollout.jsonl");
        let rows = [
            json!({"type":"session_meta","payload":{"id":"thread-1"}}),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"hello"}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1",
                "error":{"message":"original failure","codex_error_info":"cyber_policy"}}}),
        ];
        fs::write(&path, rows.iter().map(Value::to_string).collect::<Vec<_>>().join("\n") + "\n")
            .expect("write rollout");
        let initial_file_size = fs::metadata(&path).unwrap().len();
        let history_path = codex_home.join("thread_history_1.sqlite");
        let history = Connection::open(&history_path).expect("create Codex history");
        history.execute_batch(
            "CREATE TABLE thread_turns (
                 thread_id TEXT NOT NULL, turn_id TEXT NOT NULL, error_json TEXT,
                 status TEXT NOT NULL, rollout_ordinal INTEGER NOT NULL,
                 rollout_end_ordinal INTEGER, rollout_byte_offset INTEGER,
                 rollout_end_byte_offset INTEGER, PRIMARY KEY (thread_id, turn_id));
             CREATE TABLE thread_history_projection_state (
                 thread_id TEXT PRIMARY KEY, next_rollout_ordinal INTEGER NOT NULL,
                 next_rollout_byte_offset INTEGER NOT NULL);",
        ).expect("create history table");
        history.execute(
            "INSERT INTO thread_turns
                (thread_id, turn_id, error_json, status, rollout_ordinal, rollout_end_ordinal)
             VALUES (?1, ?2, ?3, 'failed', 1, 2)",
            rusqlite::params!["thread-1", "turn-1", json!({
                "message":"original failure", "codexErrorInfo":"cyberPolicy"
            }).to_string()],
        ).expect("insert history turn");
        history.execute(
            "INSERT INTO thread_history_projection_state VALUES (?1, 3, 999999)",
            ["thread-1"],
        ).expect("insert stale projection state");
        drop(history);

        let db = crate::database::DbState::new(root.join("memory-forge.db").to_str().unwrap())
            .expect("open forge database");
        crate::database::init_tables(&db.conn.lock().unwrap()).expect("init forge database");
        let settings = crate::settings::AppSettings {
            codex_home: Some(codex_home.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let platform = CodexPlatform::new(codex_home, None);
        let key = path.to_str().unwrap();
        let edit_target = format!("{key}::2::task_error");

        let invalid_target = format!("{key}::1::task_error");
        assert!(platform.update_message(&invalid_target, "bad").is_err());
        #[cfg(target_os = "linux")]
        {
            let writer = OpenOptions::new().write(true).open(&path).unwrap();
            assert!(platform.update_message(&edit_target, &"long error ".repeat(30))
                .unwrap_err().contains("Codex is writing"));
            drop(writer);
            assert_eq!(fs::metadata(&path).unwrap().len(), initial_file_size);
        }
        crate::session_service::session_edit_message(
            &db, &settings, "codex", &edit_target, "edited failure", key,
        ).expect("edit turn error");
        assert_eq!(platform.read_jsonl(&path)[2]["payload"]["error"]["message"], "edited failure");
        let history = Connection::open(&history_path).unwrap();
        let projected: String = history.query_row(
            "SELECT error_json FROM thread_turns WHERE thread_id = ?1 AND turn_id = ?2",
            ["thread-1", "turn-1"], |row| row.get(0),
        ).unwrap();
        let projected: Value = serde_json::from_str(&projected).unwrap();
        assert_eq!(projected["message"], "edited failure");
        assert_eq!(projected["codexErrorInfo"], "cyberPolicy");
        let index = SessionContentIndex::new(&db.conn);
        assert!(index.search_platform("codex", "edited failure", 5).unwrap().contains_key(key));
        assert!(!index.search_platform("codex", "original failure", 5).unwrap().contains_key(key));

        crate::session_service::session_edit_message(
            &db, &settings, "codex", &edit_target, "", key,
        ).expect("erase turn error");
        assert_eq!(fs::metadata(&path).unwrap().len(), initial_file_size);
        assert_eq!(platform.get_session_detail(key, &HashMap::new()).unwrap().blocks.len(), 1);
        assert!(platform.read_jsonl(&path)[2]["payload"]["error"].is_null());
        let erased: (Option<String>, String, i64) = history.query_row(
            "SELECT error_json, status, rollout_end_byte_offset FROM thread_turns WHERE thread_id = ?1 AND turn_id = ?2",
            ["thread-1", "turn-1"], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).unwrap();
        assert_eq!((erased.0, erased.1.as_str()), (None, "completed"));
        assert_eq!(erased.2, fs::metadata(&path).unwrap().len() as i64);
        let projection_offset: i64 = history.query_row(
            "SELECT next_rollout_byte_offset FROM thread_history_projection_state WHERE thread_id = ?1",
            ["thread-1"], |row| row.get(0),
        ).unwrap();
        assert_eq!(projection_offset, erased.2);
        assert!(!index.search_platform("codex", "edited failure", 5).unwrap().contains_key(key));
        let logs = crate::database::get_edit_log(&db.conn, "codex", key).unwrap();
        assert_eq!(logs.len(), 2);
        let erase_id = logs.iter().find(|log| log.new_content.is_empty()).unwrap().id;
        crate::session_service::session_restore_message(&db, &settings, "codex", erase_id, key)
            .expect("restore erased turn error");
        assert_eq!(fs::metadata(&path).unwrap().len(), initial_file_size);
        assert_eq!(platform.get_session_detail(key, &HashMap::new()).unwrap().blocks[1].content, "edited failure");
        let projected: String = history.query_row(
            "SELECT error_json FROM thread_turns WHERE thread_id = ?1 AND turn_id = ?2",
            ["thread-1", "turn-1"], |row| row.get(0),
        ).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&projected).unwrap()["message"], "edited failure");
        let restored_status: String = history.query_row(
            "SELECT status FROM thread_turns WHERE thread_id = ?1 AND turn_id = ?2",
            ["thread-1", "turn-1"], |row| row.get(0),
        ).unwrap();
        assert_eq!(restored_status, "failed");
        assert!(index.search_platform("codex", "edited failure", 5).unwrap().contains_key(key));
        drop(history);
        drop(index);
        drop(db);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn turn_error_edit_rolls_back_jsonl_if_history_message_has_changed() {
        let root = std::env::temp_dir().join(format!(
            "memory-forge-codex-turn-mismatch-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("rollout.jsonl");
        let original = [
            json!({"payload":{"id":"thread-1"}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1",
                "error":{"message":"old failure"}}}),
        ].iter().map(Value::to_string).collect::<Vec<_>>().join("\n") + "\n";
        fs::write(&path, &original).unwrap();
        let history = Connection::open(root.join("thread_history_1.sqlite")).unwrap();
        history.execute_batch("CREATE TABLE thread_turns (thread_id TEXT, turn_id TEXT, error_json TEXT);
            INSERT INTO thread_turns VALUES ('thread-1', 'turn-1', '{\"message\":\"different failure\"}');")
            .unwrap();
        drop(history);
        let platform = CodexPlatform::new(root.clone(), None);
        let target = format!("{}::1::task_error", path.display());
        assert!(platform.update_message(&target, "replacement").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn parses_top_level_subagent_metadata_and_legacy_role_alias() {
        let metadata = codex_agent_metadata(&json!({
            "parent_thread_id": "parent",
            "agent_nickname": "Scout",
            "agent_type": "explorer",
            "agent_path": "root/scout",
            "thread_source": "subagent"
        }))
        .unwrap();

        assert_eq!(metadata.parent_session_id.as_deref(), Some("parent"));
        assert_eq!(metadata.nickname.as_deref(), Some("Scout"));
        assert_eq!(metadata.role.as_deref(), Some("explorer"));
        assert_eq!(metadata.path.as_deref(), Some("root/scout"));
    }

    #[test]
    fn parses_nested_thread_spawn_metadata() {
        let metadata = codex_agent_metadata(&json!({
            "source": { "subagent": { "thread_spawn": {
                "parent_thread_id": "parent",
                "depth": 2,
                "agent_nickname": "Builder",
                "agent_role": "worker",
                "agent_path": "root/builder"
            } } }
        }))
        .unwrap();

        assert_eq!(metadata.parent_session_id.as_deref(), Some("parent"));
        assert_eq!(metadata.depth, Some(2));
        assert_eq!(metadata.nickname.as_deref(), Some("Builder"));
        assert_eq!(metadata.role.as_deref(), Some("worker"));
    }

    #[test]
    fn agent_metadata_scan_skips_malformed_jsonl_rows() {
        let root = std::env::var("MEMORY_FORGE_TEST_TMP")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir())
            .join(format!("codex-agent-scan-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create test dir");
        let path = root.join("session.jsonl");
        fs::write(
            &path,
            "not json\n{\"payload\":{\"parent_thread_id\":\"parent\",\"thread_source\":\"subagent\"}}\n",
        )
        .expect("write session");

        let platform = CodexPlatform::new(root.clone(), None);
        let metadata = platform.scan_agent_metadata(&path).unwrap();
        fs::remove_dir_all(root).ok();

        assert_eq!(metadata.parent_session_id.as_deref(), Some("parent"));
    }

    #[test]
    fn blocks_merge_function_call_and_output_on_following_agent_message() {
        let platform = CodexPlatform::new(PathBuf::from("unused"), None);
        let lines = vec![
            json!({ "payload": { "type": "user_message", "message": "run tests" } }),
            json!({ "payload": { "type": "function_call", "call_id": "call_1", "name": "shell", "arguments": { "command": "cargo test" } } }),
            json!({ "payload": { "type": "function_call_output", "call_id": "call_1", "output": "ok" } }),
            json!({ "payload": { "type": "agent_message", "message": "Tests passed." } }),
        ];

        let blocks = platform.blocks(&lines, "session.jsonl");

        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].tool_calls.is_empty());
        assert_eq!(blocks[1].role, "assistant");
        assert_eq!(blocks[1].tool_calls.len(), 1);
        assert_eq!(blocks[1].tool_calls[0].name, "shell");
        assert_eq!(blocks[1].tool_calls[0].kind, "function_call");
        assert_eq!(blocks[1].tool_calls[0].status, "completed");
        assert_eq!(
            blocks[1].tool_calls[0].input.as_deref(),
            Some("{\n  \"command\": \"cargo test\"\n}")
        );
        assert_eq!(blocks[1].tool_calls[0].output.as_deref(), Some("ok"));
    }

    #[test]
    fn blocks_attach_pending_function_calls_to_previous_agent_before_user_message() {
        let platform = CodexPlatform::new(PathBuf::from("unused"), None);
        let lines = vec![
            json!({ "payload": { "type": "agent_message", "message": "I will inspect it." } }),
            json!({ "payload": { "type": "function_call", "call_id": "call_1", "name": "shell", "arguments": { "command": "rg TODO" } } }),
            json!({ "payload": { "type": "function_call_output", "call_id": "call_1", "output": "todo found" } }),
            json!({ "payload": { "type": "user_message", "message": "continue" } }),
        ];

        let blocks = platform.blocks(&lines, "session.jsonl");

        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].role, "assistant");
        assert_eq!(blocks[0].tool_calls.len(), 1);
        assert_eq!(blocks[0].tool_calls[0].name, "shell");
        assert_eq!(blocks[0].tool_calls[0].status, "completed");
        assert_eq!(
            blocks[0].tool_calls[0].input.as_deref(),
            Some("{\n  \"command\": \"rg TODO\"\n}")
        );
        assert_eq!(
            blocks[0].tool_calls[0].output.as_deref(),
            Some("todo found")
        );
        assert_eq!(blocks[1].role, "user");
        assert!(blocks[1].tool_calls.is_empty());
    }

    #[test]
    fn blocks_parse_response_messages_reasoning_and_skip_mirrored_events() {
        let platform = CodexPlatform::new(PathBuf::from("unused"), None);
        let lines = vec![
            json!({ "payload": { "type": "message", "role": "developer", "content": [{ "type": "input_text", "text": "developer" }] } }),
            json!({ "payload": { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "<environment_context>\nignored" }] } }),
            json!({ "payload": { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "fix it" }] } }),
            json!({ "payload": { "type": "user_message", "message": "fix it" } }),
            json!({ "payload": { "type": "reasoning", "content": null, "summary": [{ "text": "checking the shape" }], "encrypted_content": "secret" } }),
            json!({ "payload": { "type": "agent_message", "message": "done" } }),
            json!({ "payload": { "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "done" }] } }),
        ];

        let blocks = platform.blocks(&lines, "session.jsonl");

        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].role, "user");
        assert_eq!(blocks[0].content, "fix it");
        assert_eq!(blocks[0].edit_target, "session.jsonl::2::0::text");
        assert_eq!(blocks[1].role, "thinking");
        assert_eq!(blocks[1].content, "checking the shape");
        assert!(!blocks[1].editable);
        assert_eq!(blocks[2].role, "assistant");
        assert_eq!(blocks[2].content, "done");
    }

    #[test]
    fn update_message_syncs_response_item_with_mirrored_event_message() {
        let root = std::env::temp_dir().join(format!(
            "memory-forge-codex-update-test-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create temp dir");
        let path = root.join("session.jsonl");
        let rows = vec![
            json!({ "payload": { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "old request" }] } }),
            json!({ "payload": { "type": "user_message", "message": "old request" } }),
            json!({ "payload": { "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "old answer" }] } }),
            json!({ "type": "event_msg", "payload": { "item": { "type": "AgentMessage", "content": [{ "type": "Text", "text": "old answer" }] } } }),
        ];
        fs::write(
            &path,
            format!(
                "{}\n",
                rows.iter()
                    .map(serde_json::to_string)
                    .collect::<Result<Vec<_>, _>>()
                    .expect("serialize rows")
                    .join("\n")
            ),
        )
        .expect("write session");

        let platform = CodexPlatform::new(PathBuf::from("unused"), None);
        let edit_target = format!("{}::0::0::text", path.to_string_lossy());
        let old = platform
            .update_message(&edit_target, "new request")
            .expect("update response item");
        let updated = platform.read_jsonl(&path);

        assert_eq!(old, "old request");
        assert_eq!(
            updated[0]["payload"]["content"][0]["text"].as_str(),
            Some("new request")
        );
        assert_eq!(
            updated[1]["payload"]["message"].as_str(),
            Some("new request")
        );

        let assistant_edit_target = format!("{}::2::0::text", path.to_string_lossy());
        let old = platform
            .update_message(&assistant_edit_target, "new answer")
            .expect("update assistant response item");
        let updated = platform.read_jsonl(&path);

        assert_eq!(old, "old answer");
        assert_eq!(
            updated[2]["payload"]["content"][0]["text"].as_str(),
            Some("new answer")
        );
        assert_eq!(
            updated[3]["payload"]["item"]["content"][0]["text"].as_str(),
            Some("new answer")
        );

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn resolves_user_projection_id_from_turn_and_content() {
        let rows = vec![
            json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "request"}],
                    "internal_chat_message_metadata_passthrough": {"turn_id": "turn-1"}
                }
            }),
            json!({
                "type": "event_msg",
                "payload": {
                    "turn_id": "turn-1",
                    "item": {
                        "type": "UserMessage",
                        "id": "projected-user-id",
                        "content": [{"type": "Text", "text": "request"}]
                    }
                }
            }),
        ];

        assert_eq!(
            find_user_history_item_id(&rows, 0, "request").as_deref(),
            Some("projected-user-id")
        );
    }

    #[test]
    fn sync_history_projection_updates_assistant_and_user_items() {
        let root = std::env::var("MEMORY_FORGE_TEST_TMP")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir())
            .join(format!("codex-history-projection-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create test dir");
        let db_path = root.join("thread_history_1.sqlite");
        fs::write(root.join("thread_history_2.sqlite"), &[]).expect("create empty history file");
        let connection = Connection::open(&db_path).expect("create history database");
        connection
            .execute_batch(
                "CREATE TABLE thread_items (
                    thread_id TEXT NOT NULL,
                    turn_id TEXT NOT NULL,
                    item_id TEXT NOT NULL,
                    rollout_ordinal INTEGER NOT NULL,
                    created_at_ms INTEGER NOT NULL,
                    item_json TEXT NOT NULL,
                    item_type TEXT NOT NULL DEFAULT '',
                    updated_at_ordinal INTEGER NOT NULL DEFAULT 0,
                    started_at_ms INTEGER,
                    completed_at_ms INTEGER,
                    PRIMARY KEY (thread_id, turn_id, item_id)
                );",
            )
            .expect("create thread_items table");
        connection
            .execute(
                "INSERT INTO thread_items (thread_id, turn_id, item_id, rollout_ordinal, created_at_ms, item_json) VALUES (?1, ?2, ?3, 1, 0, ?4)",
                rusqlite::params![
                    "thread-1",
                    "turn-1",
                    "assistant-1",
                    json!({"type":"agentMessage","id":"assistant-1","text":"old answer"})
                        .to_string()
                ],
            )
            .expect("insert assistant projection");
        connection
            .execute(
                "INSERT INTO thread_items (thread_id, turn_id, item_id, rollout_ordinal, created_at_ms, item_json) VALUES (?1, ?2, ?3, 2, 0, ?4)",
                rusqlite::params![
                    "thread-1",
                    "turn-1",
                    "user-1",
                    json!({"type":"userMessage","id":"user-1","content":[{"type":"text","text":"old request"}]})
                        .to_string()
                ],
            )
            .expect("insert user projection");
        drop(connection);

        assert_eq!(
            sync_history_projection(
                &root,
                "thread-1",
                "assistant-1",
                "assistant",
                "old answer",
                "new answer"
            )
            .expect("sync assistant projection"),
            1
        );
        assert_eq!(
            sync_history_projection(
                &root,
                "thread-1",
                "user-1",
                "user",
                "old request",
                "new request"
            )
            .expect("sync user projection"),
            1
        );

        let connection = Connection::open(&db_path).expect("open history database");
        let assistant: String = connection
            .query_row(
                "SELECT item_json FROM thread_items WHERE thread_id = ?1 AND item_id = ?2",
                ("thread-1", "assistant-1"),
                |row| row.get(0),
            )
            .expect("read assistant projection");
        let user: String = connection
            .query_row(
                "SELECT item_json FROM thread_items WHERE thread_id = ?1 AND item_id = ?2",
                ("thread-1", "user-1"),
                |row| row.get(0),
            )
            .expect("read user projection");
        assert_eq!(serde_json::from_str::<Value>(&assistant).unwrap()["text"], "new answer");
        assert_eq!(
            serde_json::from_str::<Value>(&user).unwrap()["content"][0]["text"],
            "new request"
        );

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn path_filter_matches_project_root_boundaries() {
        assert!(path_is_within_root(
            r"F:\workspacevk\project-a",
            Path::new(r"F:\workspacevk")
        ));
        assert!(path_is_within_root(
            r"F:\workspacevk",
            Path::new(r"F:\workspacevk")
        ));
        assert!(!path_is_within_root(
            r"F:\workspacevk-other\project-a",
            Path::new(r"F:\workspacevk")
        ));
    }

    #[test]
    fn list_sessions_filters_by_configured_project_root() {
        let root = std::env::temp_dir().join(format!(
            "memory-forge-codex-root-test-{}",
            std::process::id()
        ));
        let codex_home = root.join("codex-home");
        let sessions_dir = codex_home.join("sessions").join("2026");
        let project_root = root.join("workspace");
        let project_a = project_root.join("project-a");
        let other_project = root.join("other").join("project-b");
        fs::create_dir_all(&sessions_dir).expect("create sessions dir");

        fs::write(
            sessions_dir.join("inside.jsonl"),
            serde_json::to_string(&json!({
                "payload": {
                    "type": "user_message",
                    "id": "inside",
                    "cwd": project_a.display().to_string(),
                    "message": "inside"
                }
            }))
            .expect("serialize inside session"),
        )
        .expect("write inside session");
        fs::write(
            sessions_dir.join("outside.jsonl"),
            serde_json::to_string(&json!({
                "payload": {
                    "type": "user_message",
                    "id": "outside",
                    "cwd": other_project.display().to_string(),
                    "message": "outside"
                }
            }))
            .expect("serialize outside session"),
        )
        .expect("write outside session");

        let platform = CodexPlatform::new(codex_home, Some(project_root));
        let result = platform.list_sessions(&HashMap::new(), None, 0);

        fs::remove_dir_all(root).ok();

        assert_eq!(result.total, 1);
        assert_eq!(result.items[0].session_id, "inside");
        assert_eq!(result.items[0].preview, "inside");
    }
}
