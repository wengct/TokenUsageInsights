use axum::{
    extract::Path,
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{BufRead, BufReader},
    path::PathBuf,
};
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use rusqlite::params;

mod db;
use db::{TokenStats, CostStats, UsageEntry};

#[tokio::main]
async fn main() {
    // 初始化 SQLite 資料庫並進行第一次增量同步與遷移
    if let Ok(conn) = db::get_db_conn() {
        if let Err(e) = db::init_db(&conn) {
            eprintln!("❌ 初始化 SQLite 資料庫失敗: {}", e);
        } else {
            // 嘗試從舊的個別資料庫遷移數據 (策略 B)
            if let Err(e) = db::migrate_old_databases(&conn) {
                eprintln!("⚠️ 數據遷移遭遇錯誤: {}", e);
            }
            if let Err(e) = db::sync_usage_logs(&conn) {
                eprintln!("❌ 初次同步日誌檔到 SQLite 失敗: {}", e);
            } else {
                println!("✅ SQLite 資料庫已成功載入並完成增量同步！");
            }
        }
    } else {
        eprintln!("❌ 無法連結到 SQLite 資料庫");
    }

    let static_dir = get_static_dir();
    println!("📂 正在服務靜態檔案，目錄來源: {:?}", static_dir);

    // 建立 Axum 路由，支援帶助理前綴的 API 及 fallback 相容 API
    let app = Router::new()
        // 帶 :assistant 變數的路由
        .route("/api/:assistant/dates", get(get_available_dates))
        .route("/api/:assistant/setup-info", get(get_setup_info))
        .route("/api/:assistant/usage/:date", get(get_usage_details))
        .route("/api/:assistant/session/:session_id", get(get_session_details))
        .route("/api/:assistant/months", get(get_available_months))
        .route("/api/:assistant/monthly/:year_month", get(get_monthly_details))
        .route("/api/:assistant/pricing", get(get_pricing))
        .route("/api/:assistant/sync", get(trigger_manual_sync))
        
        // 靜態檔案路由
        .nest_service("/static", ServeDir::new(&static_dir))
        .fallback_service(ServeDir::new(&static_dir))
        .layer(CorsLayer::permissive());

    let port = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(3003); // 預設使用 3003 Port

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await.unwrap();
    println!("🚀 CLI Token Usage Insights Dashboard is running on: http://localhost:{}", port);
    
    axum::serve(listener, app).await.unwrap();
}

/// 獲取靜態檔案的基準路徑
fn get_static_dir() -> PathBuf {
    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
        let p = PathBuf::from(manifest_dir).join("static");
        if p.exists() { return p; }
    }
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            let p = exe_dir.join("static");
            if p.exists() { return p; }
            if let Some(grandparent) = exe_dir.parent().and_then(|p| p.parent()) {
                let p = grandparent.join("static");
                if p.exists() { return p; }
            }
        }
    }
    let pwd = PathBuf::from("static");
    if pwd.exists() { return pwd; }
    
    if let Ok(cwd) = std::env::current_dir() {
        let p = cwd.join("static");
        if p.exists() { return p; }
    }
    eprintln!("❌ 無法定位 static 目錄。請在專案根目錄下執行此程式。");
    std::process::exit(1);
}

// =========================================================================
// API 核心輔助結構與定價引擎
// =========================================================================

#[derive(Serialize)]
struct DateListResponse {
    dates: Vec<String>,
}

#[derive(Serialize)]
struct MonthListResponse {
    months: Vec<String>,
}

#[derive(Serialize)]
struct SetupInfoResponse {
    workspace_dir: String,
    home_dir: String,
    antigravity: AssistantSetupStatus,
    copilot: AssistantSetupStatus,
    codex: AssistantSetupStatus,
}

#[derive(Serialize)]
struct AssistantSetupStatus {
    dir_path: String,
    exists: bool,
    script_path: String,
}

#[derive(Serialize, Default, Clone)]
struct DaySummary {
    total_sessions: usize,
    total_tokens: u64,
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_cache_read_tokens: u64,
    total_reasoning_tokens: u64,
    total_duration_ms: u64,
    total_requests: u64,
    total_cost_usd: f64,
}

#[derive(Serialize, Clone)]
struct SessionSummary {
    session_id: String,
    session_name: String,
    assistant_type: String,
    cwd: String,
    model: String,
    total_tokens: u64,
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_cache_read_tokens: u64,
    total_reasoning_tokens: u64,
    max_turn_no: u32,
    timestamp: String,
    duration_ms: u64,
    cost_usd: f64,
    parent_session_id: Option<String>,
    agent_nickname: Option<String>,
    agent_role: Option<String>,
    reasoning_effort: Option<String>,
}

#[derive(Serialize)]
struct UsageDetailsResponse {
    date: String,
    summary: DaySummary,
    sessions: Vec<SessionSummary>,
    raw_entries: Vec<UsageEntry>,
}

#[derive(Debug, Clone)]
struct PricingRule {
    model_name: String,
    input_price: f64,
    cache_input_price: f64,
    output_price: f64,
}

#[derive(Serialize)]
struct PricingEntry {
    model_name: String,
    deployment_type: String,
    unit: String,
    input_price: f64,
    cache_input_price: f64,
    output_price: f64,
    batch_api_price: String,
}

fn load_pricing_rules() -> Vec<PricingRule> {
    let mut rules = Vec::new();
    let file_path = PathBuf::from("pricing.csv");
    if let Ok(file) = File::open(&file_path) {
        let reader = BufReader::new(file);
        let mut lines = reader.lines();
        if let Some(Ok(_header)) = lines.next() {
            for line in lines.flatten() {
                let parts: Vec<&str> = line.split(',').collect();
                if parts.len() >= 6 {
                    let input_price = parts[3].trim().parse::<f64>().unwrap_or(0.0);
                    let cache_input_price = parts[4].trim().parse::<f64>().unwrap_or(0.0);
                    let output_price = parts[5].trim().parse::<f64>().unwrap_or(0.0);
                    rules.push(PricingRule {
                        model_name: parts[0].trim().to_string(),
                        input_price,
                        cache_input_price,
                        output_price,
                    });
                }
            }
        }
    }
    if rules.is_empty() {
        rules = vec![
            PricingRule { model_name: "Gemini 3.5 Flash".to_string(), input_price: 0.075, cache_input_price: 0.01875, output_price: 0.30 },
            PricingRule { model_name: "Gemini 3.5 Pro".to_string(), input_price: 1.25, cache_input_price: 0.3125, output_price: 5.00 },
        ];
    }
    rules
}

fn normalize_model_name(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

fn calculate_cost(rules: &[PricingRule], model_name: &str, input: u64, output: u64, cache_read: u64) -> f64 {
    let m_norm = normalize_model_name(model_name);
    if m_norm.is_empty() {
        return 0.0;
    }
    
    let rule = rules.iter().find(|r| {
        let r_norm = normalize_model_name(&r.model_name);
        if r_norm.is_empty() {
            false
        } else {
            m_norm.contains(&r_norm) || r_norm.contains(&m_norm)
        }
    });

    if let Some(r) = rule {
        let input_cost = (input as f64 / 1_000_000.0) * r.input_price;
        let cache_cost = (cache_read as f64 / 1_000_000.0) * r.cache_input_price;
        let output_cost = (output as f64 / 1_000_000.0) * r.output_price;
        input_cost + cache_cost + output_cost
    } else {
        0.0 // 未知模型暫估為 0 元
    }
}

// =========================================================================
// API 路由處理常式
// =========================================================================

/// API 1: 獲取可用的有使用記錄日期
async fn get_available_dates(Path(assistant): Path<String>) -> impl IntoResponse {
    let _ = tokio::task::spawn_blocking(|| {
        if let Ok(conn) = db::get_db_conn() {
            let _ = db::sync_usage_logs(&conn);
        }
    }).await;

    let res: Result<Vec<String>, String> = tokio::task::spawn_blocking(move || {
        let conn = db::get_db_conn()?;
        let mut dates = Vec::new();
        if assistant == "all" {
            let mut stmt = conn.prepare("SELECT DISTINCT date FROM usage_entries ORDER BY date DESC").map_err(|e| e.to_string())?;
            let date_iter = stmt.query_map([], |row| row.get::<_, String>(0)).map_err(|e| e.to_string())?;
            for d in date_iter {
                dates.push(d.map_err(|e| e.to_string())?);
            }
        } else {
            let assistants: Vec<&str> = assistant.split(',').collect();
            let mut placeholders = Vec::new();
            let mut params_vec = Vec::new();
            for a in assistants {
                placeholders.push("?");
                params_vec.push(rusqlite::types::Value::Text(a.to_string()));
            }
            let query = format!(
                "SELECT DISTINCT date FROM usage_entries WHERE assistant_type IN ({}) ORDER BY date DESC",
                placeholders.join(",")
            );
            let mut stmt = conn.prepare(&query).map_err(|e| e.to_string())?;
            let date_iter = stmt.query_map(rusqlite::params_from_iter(params_vec), |row| row.get::<_, String>(0)).map_err(|e| e.to_string())?;
            for d in date_iter {
                dates.push(d.map_err(|e| e.to_string())?);
            }
        }
        Ok(dates)
    }).await.unwrap_or_else(|_| Err("執行緒執行失敗".to_string()));

    match res {
        Ok(date_list) => Json(DateListResponse { dates: date_list }).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e }))).into_response(),
    }
}

/// API 2: 獲取當前環境配置與安裝狀況資訊
async fn get_setup_info(Path(_assistant): Path<String>) -> impl IntoResponse {
    let workspace_dir = match std::env::current_dir() {
        Ok(dir) => dir.to_string_lossy().into_owned(),
        Err(_) => "".to_string(),
    };
    let home_dir_path = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/home/chenting"));
    let home_dir = home_dir_path.to_string_lossy().into_owned();

    let anti_dir = db::get_antigravity_dir();
    let anti_script = anti_dir.join("statusline-token.sh").to_string_lossy().into_owned();
    let anti_exists = anti_dir.exists();

    let copilot_dir = db::get_copilot_dir();
    let copilot_script = copilot_dir.join("statusline-token.sh").to_string_lossy().into_owned();
    let copilot_exists = copilot_dir.exists();

    let codex_dir = db::get_codex_dir();
    let codex_exists = codex_dir.exists();

    Json(SetupInfoResponse {
        workspace_dir,
        home_dir,
        antigravity: AssistantSetupStatus { dir_path: anti_dir.to_string_lossy().into_owned(), exists: anti_exists, script_path: anti_script },
        copilot: AssistantSetupStatus { dir_path: copilot_dir.to_string_lossy().into_owned(), exists: copilot_exists, script_path: copilot_script },
        codex: AssistantSetupStatus { dir_path: codex_dir.to_string_lossy().into_owned(), exists: codex_exists, script_path: "".to_string() },
    })
}

/// API 3: 獲取指定日期的 Token 使用詳情與會話列表
async fn get_usage_details(Path((assistant, date)): Path<(String, String)>) -> impl IntoResponse {
    let _ = tokio::task::spawn_blocking(|| {
        if let Ok(conn) = db::get_db_conn() {
            let _ = db::sync_usage_logs(&conn);
        }
    }).await;

    let assistant_clone = assistant.clone();
    let date_clone = date.clone();

    let entries_res: Result<Vec<(UsageEntry, String)>, String> = tokio::task::spawn_blocking(move || {
        let conn = db::get_db_conn()?;
        let mut query = "SELECT 
                timestamp, session_id, session_name, transcript_path, cwd, version, turn_no, model, model_id,
                tokens_input, tokens_output, tokens_cache_read, tokens_reasoning, tokens_total,
                delta_input, delta_output, delta_cache_read, delta_reasoning, delta_total,
                duration_ms, premium_requests, parent_session_id, agent_nickname, agent_role, assistant_type, reasoning_effort
             FROM usage_entries WHERE date = ?".to_string();
        let mut params_vec = Vec::new();
        params_vec.push(rusqlite::types::Value::Text(date_clone));

        if assistant_clone != "all" {
            let assistants: Vec<&str> = assistant_clone.split(',').collect();
            let mut placeholders = Vec::new();
            for a in assistants {
                placeholders.push("?");
                params_vec.push(rusqlite::types::Value::Text(a.to_string()));
            }
            query.push_str(&format!(" AND assistant_type IN ({})", placeholders.join(",")));
        }
        query.push_str(" ORDER BY timestamp ASC");

        let mut stmt = conn.prepare(&query).map_err(|e| e.to_string())?;
        let mut rows = stmt.query(rusqlite::params_from_iter(params_vec)).map_err(|e| e.to_string())?;

        let mut entries = Vec::new();
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let ast_type = row.get::<_, String>(24).map_err(|e| e.to_string())?;
            let tokens_input: Option<u64> = row.get::<_, Option<i64>>(9).map_err(|e| e.to_string())?.map(|v| v as u64);
            let tokens_output: Option<u64> = row.get::<_, Option<i64>>(10).map_err(|e| e.to_string())?.map(|v| v as u64);
            let tokens_cache_read: Option<u64> = row.get::<_, Option<i64>>(11).map_err(|e| e.to_string())?.map(|v| v as u64);
            let tokens_reasoning: Option<u64> = row.get::<_, Option<i64>>(12).map_err(|e| e.to_string())?.map(|v| v as u64);
            let tokens_total: Option<u64> = row.get::<_, Option<i64>>(13).map_err(|e| e.to_string())?.map(|v| v as u64);

            let tokens = if let (Some(input), Some(output), Some(total)) = (tokens_input, tokens_output, tokens_total) {
                Some(TokenStats { input, output, cache_read: tokens_cache_read, cache_write: None, reasoning: tokens_reasoning, total })
            } else {
                None
            };

            let delta_input: Option<u64> = row.get::<_, Option<i64>>(14).map_err(|e| e.to_string())?.map(|v| v as u64);
            let delta_output: Option<u64> = row.get::<_, Option<i64>>(15).map_err(|e| e.to_string())?.map(|v| v as u64);
            let delta_cache_read: Option<u64> = row.get::<_, Option<i64>>(16).map_err(|e| e.to_string())?.map(|v| v as u64);
            let delta_reasoning: Option<u64> = row.get::<_, Option<i64>>(17).map_err(|e| e.to_string())?.map(|v| v as u64);
            let delta_total: Option<u64> = row.get::<_, Option<i64>>(18).map_err(|e| e.to_string())?.map(|v| v as u64);

            let delta_tokens = if let (Some(input), Some(output), Some(total)) = (delta_input, delta_output, delta_total) {
                Some(TokenStats { input, output, cache_read: delta_cache_read, cache_write: None, reasoning: delta_reasoning, total })
            } else {
                None
            };

            let duration_ms: Option<f64> = row.get::<_, Option<i64>>(19).map_err(|e| e.to_string())?.map(|v| v as f64);
            let premium_requests: Option<f64> = row.get::<_, Option<i64>>(20).map_err(|e| e.to_string())?.map(|v| v as f64);

            let cost = if duration_ms.is_some() || premium_requests.is_some() {
                Some(CostStats { total_api_duration_ms: duration_ms, total_duration_ms: None, total_premium_requests: premium_requests })
            } else {
                None
            };

            entries.push((UsageEntry {
                timestamp: row.get(0).map_err(|e| e.to_string())?,
                session_id: row.get(1).map_err(|e| e.to_string())?,
                session_name: row.get(2).ok(),
                transcript_path: row.get(3).ok(),
                cwd: row.get(4).ok(),
                version: row.get(5).ok(),
                turn_no: row.get::<_, i64>(6).map_err(|e| e.to_string())? as u32,
                model: row.get(7).ok(),
                model_id: row.get(8).ok(),
                tokens,
                delta_tokens,
                context: None,
                cost,
                parent_session_id: row.get(21).ok(),
                agent_nickname: row.get(22).ok(),
                agent_role: row.get(23).ok(),
                reasoning_effort: row.get(25).ok(),
            }, ast_type));
        }
        Ok(entries)
    }).await.unwrap_or_else(|_| Err("執行緒執行失敗".to_string()));

    let entries_with_type = match entries_res {
        Ok(e) => e,
        Err(err) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": err }))).into_response(),
    };

    if entries_with_type.is_empty() {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "找不到該日期的使用量資料。" }))).into_response();
    }

    let mut summary = DaySummary::default();
    let mut sessions_map: HashMap<String, (Vec<UsageEntry>, String)> = HashMap::new();
    let mut entries = Vec::new();

    for (e, ast_type) in &entries_with_type {
        entries.push(e.clone());
        let (list, _) = sessions_map.entry(e.session_id.clone()).or_insert_with(|| (Vec::new(), ast_type.clone()));
        list.push(e.clone());
    }

    summary.total_sessions = sessions_map.len();
    let mut session_last_entries: HashMap<String, UsageEntry> = HashMap::new();

    for e in &entries {
        if let Some(ref tokens) = e.delta_tokens {
            summary.total_tokens += tokens.total;
            summary.total_input_tokens += tokens.input;
            summary.total_output_tokens += tokens.output;
            summary.total_cache_read_tokens += tokens.cache_read.unwrap_or(0);
            summary.total_reasoning_tokens += tokens.reasoning.unwrap_or(0);
        } else if let Some(ref tokens) = e.tokens {
            if e.turn_no == 1 {
                summary.total_tokens += tokens.total;
                summary.total_input_tokens += tokens.input;
                summary.total_output_tokens += tokens.output;
                summary.total_cache_read_tokens += tokens.cache_read.unwrap_or(0);
                summary.total_reasoning_tokens += tokens.reasoning.unwrap_or(0);
            }
        }

        let sid = e.session_id.clone();
        let last_e = session_last_entries.entry(sid).or_insert_with(|| e.clone());
        if e.turn_no > last_e.turn_no {
            *last_e = e.clone();
        }
    }

    let pricing_rules = load_pricing_rules();
    let mut sessions_summary = Vec::new();

    for (session_id, (s_entries, ast_type)) in &sessions_map {
        let last_entry = session_last_entries.get(session_id).cloned().unwrap_or_else(|| s_entries[0].clone());
        
        let session_tokens = s_entries.iter().map(|e| e.delta_tokens.as_ref().map(|t| t.total).unwrap_or(0)).sum::<u64>();
        let session_input_tokens = s_entries.iter().map(|e| e.delta_tokens.as_ref().map(|t| t.input).unwrap_or(0)).sum::<u64>();
        let session_output_tokens = s_entries.iter().map(|e| e.delta_tokens.as_ref().map(|t| t.output).unwrap_or(0)).sum::<u64>();
        let session_cache_read = s_entries.iter().map(|e| e.delta_tokens.as_ref().and_then(|t| t.cache_read).unwrap_or(0)).sum::<u64>();
        let session_reasoning = s_entries.iter().map(|e| e.delta_tokens.as_ref().and_then(|t| t.reasoning).unwrap_or(0)).sum::<u64>();

        let session_duration = last_entry.cost.as_ref().and_then(|c| c.total_api_duration_ms).unwrap_or(0.0) as u64;
        let session_requests = last_entry.cost.as_ref().and_then(|c| c.total_premium_requests).unwrap_or(0.0) as u64;

        summary.total_duration_ms += session_duration;
        summary.total_requests += session_requests;

        let total_cache_read_tokens = if session_tokens > 0 { session_cache_read } else { last_entry.tokens.as_ref().and_then(|t| t.cache_read).unwrap_or(0) };
        let total_reasoning_tokens = if session_tokens > 0 { session_reasoning } else { last_entry.tokens.as_ref().and_then(|t| t.reasoning).unwrap_or(0) };
        let total_input_tokens = if session_tokens > 0 { session_input_tokens } else { last_entry.tokens.as_ref().map(|t| t.input).unwrap_or(0) };
        let total_output_tokens = if session_tokens > 0 { session_output_tokens } else { last_entry.tokens.as_ref().map(|t| t.output).unwrap_or(0) };

        let cost_usd = calculate_cost(
            &pricing_rules,
            &last_entry.model.clone().unwrap_or_else(|| "Unknown Model".to_string()),
            total_input_tokens,
            total_output_tokens,
            total_cache_read_tokens,
        );
        summary.total_cost_usd += cost_usd;

        sessions_summary.push(SessionSummary {
            session_id: session_id.clone(),
            session_name: last_entry.session_name.unwrap_or_else(|| "Start Coding Session".to_string()),
            assistant_type: ast_type.clone(),
            cwd: last_entry.cwd.unwrap_or_default(),
            model: last_entry.model.unwrap_or_else(|| "Unknown Model".to_string()),
            total_tokens: if session_tokens > 0 { session_tokens } else { last_entry.tokens.as_ref().map(|t| t.total).unwrap_or(0) },
            total_input_tokens,
            total_output_tokens,
            total_cache_read_tokens,
            total_reasoning_tokens,
            max_turn_no: s_entries.iter().map(|e| e.turn_no).max().unwrap_or(1),
            timestamp: s_entries[0].timestamp.clone(),
            duration_ms: session_duration,
            cost_usd,
            parent_session_id: last_entry.parent_session_id.clone(),
            agent_nickname: last_entry.agent_nickname.clone(),
            agent_role: last_entry.agent_role.clone(),
            reasoning_effort: last_entry.reasoning_effort.clone(),
        });
    }

    sessions_summary.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

    Json(UsageDetailsResponse {
        date,
        summary,
        sessions: sessions_summary,
        raw_entries: entries,
    }).into_response()
}

/// Timeline Item definition for Session Reconstruction
#[derive(Serialize)]
#[serde(tag = "type")]
enum TimelineItem {
    UserPrompt {
        timestamp: String,
        prompt: String,
        context: Option<serde_json::Value>,
        turn_no: u32,
    },
    AgentReply {
        timestamp: String,
        reply: String,
        reasoning: Option<String>,
        turn_no: u32,
        model: String,
        tokens: Option<TokenStats>,
        duration_ms: Option<u64>,
        reasoning_effort: Option<String>,
    },
    ToolStep {
        timestamp: String,
        tool_name: String,
        arguments: serde_json::Value,
        env: Option<serde_json::Value>,
        exit_code: Option<i32>,
        stdout: String,
        stderr: String,
        tool_call_id: Option<String>,
        status: String, // 'running', 'success', 'failed'
    },
    SystemStatus {
        timestamp: String,
        status_type: String, // 'session_start', 'session_end', 'compaction', etc.
        message: String,
    },
}

/// API 4: 獲取特定會話的詳細對話歷史還原時間軸
async fn get_session_details(Path((assistant, session_id)): Path<(String, String)>) -> impl IntoResponse {
    // 1. 若 assistant == "all"，需要先在資料庫尋找此 session 屬於哪一個 assistant，並獲取 transcript 路徑
    let session_info: Result<(String, Option<String>), String> = tokio::task::spawn_blocking({
        let sid = session_id.clone();
        move || {
            let conn = db::get_db_conn()?;
            let mut stmt = conn.prepare("SELECT assistant_type, transcript_path FROM usage_entries WHERE session_id = ? LIMIT 1").map_err(|e| e.to_string())?;
            let mut rows = stmt.query(params![sid]).map_err(|e| e.to_string())?;
            if let Some(row) = rows.next().map_err(|e| e.to_string())? {
                let ast: String = row.get(0).map_err(|e| e.to_string())?;
                let path: Option<String> = row.get(1).ok();
                Ok((ast, path))
            } else {
                Err("Session not found".to_string())
            }
        }
    }).await.unwrap_or_else(|_| Err("執行緒執行失敗".to_string()));

    let (resolved_assistant, transcript_path_db) = match session_info {
        Ok(info) => info,
        Err(_) => (assistant.clone(), None), // 若查不到則採用路徑參數
    };

    // 2. 準備讀取檔案的完整路徑
    let filepath = match resolved_assistant.as_str() {
        "antigravity" => {
            let anti_dir = db::get_antigravity_dir();
            anti_dir.join("brain").join(&session_id).join(".system_generated/logs/transcript_full.jsonl")
        }
        "copilot" => {
            let cop_dir = db::get_copilot_dir();
            let mut path = cop_dir.join("session-state").join(&session_id).join("events.jsonl");
            if !path.exists() {
                path = cop_dir.join("session-state").join(format!("{}.jsonl", session_id));
            }
            path
        }
        "codex" => {
            if let Some(ref p_str) = transcript_path_db {
                PathBuf::from(p_str)
            } else {
                return (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "找不到 Codex 會話日誌檔案路徑。" }))).into_response();
            }
        }
        _ => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "不支援的助理類型" }))).into_response(),
    };

    if !filepath.exists() {
        // 判斷是否為「尚未開始交談」（session 目錄存在但 events.jsonl 尚未產生）
        let is_session_dir_present = match resolved_assistant.as_str() {
            "copilot" => {
                let cop_dir = db::get_copilot_dir();
                cop_dir.join("session-state").join(&session_id).exists()
            }
            _ => false,
        };
        let reason = if is_session_dir_present { "no_events_yet" } else { "file_missing" };
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": format!("找不到該會話的本地日誌檔: {:?}", filepath), "reason": reason }))).into_response();
    }

    let file = match File::open(&filepath) {
        Ok(f) => f,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": format!("開啟日誌檔案失敗: {}", e) }))).into_response(),
    };

    // 3. 預先載入 SQLite 中的回合 (turn_no) 增量 token 數據
    let sid_clone = session_id.clone();
    let db_entries: HashMap<u32, (TokenStats, String)> = tokio::task::spawn_blocking(move || {
        let mut map = HashMap::new();
        if let Ok(conn) = db::get_db_conn() {
            if let Ok(mut stmt) = conn.prepare(
                "SELECT turn_no, delta_input, delta_output, delta_cache_read, delta_reasoning, delta_total, model 
                 FROM usage_entries WHERE session_id = ? ORDER BY turn_no ASC"
            ) {
                if let Ok(mut rows) = stmt.query(params![sid_clone]) {
                    while let Ok(Some(row)) = rows.next() {
                        if let (Ok(turn_no), Ok(delta_input), Ok(delta_output), Ok(delta_total)) = (
                            row.get::<_, i64>(0),
                            row.get::<_, Option<i64>>(1),
                            row.get::<_, Option<i64>>(2),
                            row.get::<_, Option<i64>>(5)
                        ) {
                            if let (Some(input), Some(output), Some(total)) = (delta_input, delta_output, delta_total) {
                                let cache_read = row.get::<_, Option<i64>>(3).ok().flatten().map(|v| v as u64);
                                let reasoning = row.get::<_, Option<i64>>(4).ok().flatten().map(|v| v as u64);
                                let model = row.get::<_, Option<String>>(6).unwrap_or(None).unwrap_or_else(|| "Gemini".to_string());
                                map.insert(turn_no as u32, (TokenStats { input: input as u64, output: output as u64, cache_read, cache_write: None, reasoning, total: total as u64 }, model));
                            }
                        }
                    }
                }
            }
        }
        map
    }).await.unwrap_or_default();

    let reader = BufReader::new(file);
    let mut timeline = Vec::new();
    let mut metadata = HashMap::new();

    // 依據不同助理格式解析日誌
    match resolved_assistant.as_str() {
        "antigravity" => {
            parse_antigravity_timeline(reader, &db_entries, &mut timeline, &mut metadata);
        }
        "copilot" => {
            parse_copilot_timeline(reader, &db_entries, &mut timeline, &mut metadata);
        }
        "codex" => {
            parse_codex_timeline(reader, &db_entries, &mut timeline, &mut metadata);
        }
        _ => {}
    }

    // 計算該會話的加總 Token 資料，供 metadata 使用
    let mut total_tokens = 0;
    let mut total_cache_read_tokens = 0;
    let mut total_input_tokens = 0;
    let mut total_output_tokens = 0;
    let mut total_reasoning_tokens = 0;

    for (_, (stats, _)) in &db_entries {
        total_tokens += stats.total;
        total_cache_read_tokens += stats.cache_read.unwrap_or(0);
        total_input_tokens += stats.input;
        total_output_tokens += stats.output;
        total_reasoning_tokens += stats.reasoning.unwrap_or(0);
    }

    metadata.insert("total_tokens".to_string(), serde_json::Value::Number(serde_json::Number::from(total_tokens)));
    metadata.insert("total_cache_read_tokens".to_string(), serde_json::Value::Number(serde_json::Number::from(total_cache_read_tokens)));
    metadata.insert("total_input_tokens".to_string(), serde_json::Value::Number(serde_json::Number::from(total_input_tokens)));
    metadata.insert("total_output_tokens".to_string(), serde_json::Value::Number(serde_json::Number::from(total_output_tokens)));
    metadata.insert("total_reasoning_tokens".to_string(), serde_json::Value::Number(serde_json::Number::from(total_reasoning_tokens)));

    #[derive(Serialize)]
    struct LegacyEventWrapper {
        event_type: String,
        event_data: serde_json::Value,
    }

    let legacy_timeline: Vec<LegacyEventWrapper> = timeline.into_iter().map(|item| {
        match item {
            TimelineItem::UserPrompt { timestamp, prompt, context, turn_no } => {
                let mut attachments = Vec::new();
                if let Some(ctx) = context {
                    if let Some(atts) = ctx.get("attachments").and_then(|a| a.as_array()) {
                        attachments = atts.clone();
                    }
                }
                LegacyEventWrapper {
                    event_type: "UserPrompt".to_string(),
                    event_data: serde_json::json!({
                        "timestamp": timestamp,
                        "prompt": prompt,
                        "transformed_prompt": None::<String>,
                        "attachments": attachments,
                        "turn_no": turn_no,
                    }),
                }
            }
            TimelineItem::AgentReply { timestamp, reply, reasoning, turn_no, model, tokens, duration_ms: _, reasoning_effort } => {
                let reply_content = if let Some(r) = reasoning {
                    format!("<details><summary>🧠 LLM Reasoning Process</summary>\n{}\n</details>\n\n{}", r, reply)
                } else {
                    reply
                };
                LegacyEventWrapper {
                    event_type: "AssistantReply".to_string(),
                    event_data: serde_json::json!({
                        "timestamp": timestamp,
                        "reply": reply_content,
                        "model": model,
                        "reasoning_effort": reasoning_effort,
                        "input_tokens": tokens.as_ref().map(|t| t.input),
                        "output_tokens": tokens.as_ref().map(|t| t.output),
                        "cache_read_tokens": tokens.as_ref().and_then(|t| t.cache_read),
                        "cache_write_tokens": tokens.as_ref().and_then(|t| t.cache_write),
                        "reasoning_tokens": tokens.as_ref().and_then(|t| t.reasoning),
                        "total_tokens": tokens.as_ref().map(|t| t.total),
                        "tool_requests": Vec::<serde_json::Value>::new(),
                        "turn_no": turn_no,
                    }),
                }
            }
            TimelineItem::ToolStep { timestamp, tool_name, arguments, env: _, exit_code, stdout, stderr, tool_call_id: _, status } => {
                let content_str = if !stderr.is_empty() {
                    format!("Stdout:\n{}\n\nStderr:\n{}", stdout, stderr)
                } else {
                    stdout
                };
                LegacyEventWrapper {
                    event_type: "ToolStep".to_string(),
                    event_data: serde_json::json!({
                        "timestamp": timestamp,
                        "tool_name": tool_name,
                        "arguments": arguments,
                        "result": if status == "success" || status == "failed" {
                            Some(serde_json::json!({
                                "content": content_str,
                                "exitCode": exit_code,
                            }))
                        } else {
                            None
                        },
                        "turn_no": 1,
                    }),
                }
            }
            TimelineItem::SystemStatus { timestamp, status_type, message } => {
                LegacyEventWrapper {
                    event_type: "SystemStatus".to_string(),
                    event_data: serde_json::json!({
                        "timestamp": timestamp,
                        "status_type": status_type,
                        "message": message,
                    }),
                }
            }
        }
    }).collect();

    Json(serde_json::json!({
        "session_id": session_id,
        "metadata": metadata,
        "timeline": legacy_timeline,
    })).into_response()
}

// =========================================================================
// 三大助理的時間軸解析副程式
// =========================================================================

fn parse_antigravity_timeline(
    reader: BufReader<File>,
    db_entries: &HashMap<u32, (TokenStats, String)>,
    timeline: &mut Vec<TimelineItem>,
    metadata: &mut HashMap<String, serde_json::Value>,
) {
    let mut turn_no = 1;
    let mut current_model = "Gemini".to_string();

    for line_res in reader.lines() {
        let line = match line_res { Ok(l) => l, Err(_) => continue };
        let step: serde_json::Value = match serde_json::from_str(&line) { Ok(v) => v, Err(_) => continue };

        let step_type = step.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let timestamp = step.get("timestamp").and_then(|t| t.as_str()).unwrap_or("").to_string();

        match step_type {
            "USER_INPUT" => {
                let content = step.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string();
                let context = step.get("context").cloned();
                timeline.push(TimelineItem::UserPrompt { timestamp, prompt: content, context, turn_no });
            }
            "PLANNER_RESPONSE" => {
                let content = step.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string();
                let reasoning = step.get("reasoning").and_then(|r| r.as_str()).map(|s| s.to_string());
                
                // 讀取該回合在 SQLite 中的增量 token
                let (tokens, model_name) = if let Some((stats, model)) = db_entries.get(&turn_no) {
                    current_model = model.clone();
                    (Some(stats.clone()), current_model.clone())
                } else {
                    (None, current_model.clone())
                };

                timeline.push(TimelineItem::AgentReply {
                    timestamp,
                    reply: content,
                    reasoning,
                    turn_no,
                    model: model_name,
                    tokens,
                    duration_ms: None,
                    reasoning_effort: None,
                });
                turn_no += 1;
            }
            "TOOL_CALL" => {
                let name = step.get("tool_name").and_then(|t| t.as_str()).unwrap_or("unknown").to_string();
                let args = step.get("arguments").cloned().unwrap_or(serde_json::Value::Null);
                let stdout = step.get("stdout").and_then(|o| o.as_str()).unwrap_or("").to_string();
                let stderr = step.get("stderr").and_then(|e| e.as_str()).unwrap_or("").to_string();
                let exit_code = step.get("exit_code").and_then(|ec| ec.as_i64()).map(|v| v as i32);
                let env = step.get("env").cloned();

                timeline.push(TimelineItem::ToolStep {
                    timestamp,
                    tool_name: name,
                    arguments: args,
                    env,
                    exit_code,
                    stdout,
                    stderr,
                    tool_call_id: None,
                    status: if exit_code.unwrap_or(0) == 0 { "success".to_string() } else { "failed".to_string() },
                });
            }
            _ => {}
        }
    }
    metadata.insert("selected_model".to_string(), serde_json::Value::String(current_model));
}

fn parse_copilot_timeline(
    reader: BufReader<File>,
    db_entries: &HashMap<u32, (TokenStats, String)>,
    timeline: &mut Vec<TimelineItem>,
    metadata: &mut HashMap<String, serde_json::Value>,
) {
    let mut current_turn_no = 1;
    let mut has_seen_user_prompt = false;
    let mut current_model = "GPT-4o".to_string();
    let mut tool_calls_map = HashMap::new();

    for line_res in reader.lines() {
        let line = match line_res { Ok(l) => l, Err(_) => continue };
        let event: serde_json::Value = match serde_json::from_str(&line) { Ok(v) => v, Err(_) => continue };

        let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let timestamp = event.get("timestamp").and_then(|t| t.as_str()).unwrap_or("").to_string();
        let payload = event.get("payload");
        let data = event.get("data");

        match event_type {
            // 舊格式
            "session_meta" | "SESSION_STARTED" => {
                let p = payload.or(data);
                if let Some(p) = p {
                    if let Some(v) = p.get("cli_version") { metadata.insert("copilot_version".to_string(), v.clone()); }
                    if let Some(cwd) = p.get("cwd") { metadata.insert("cwd".to_string(), cwd.clone()); }
                    if let Some(git) = p.get("git") {
                        if let Some(branch) = git.get("branch") { metadata.insert("git_branch".to_string(), branch.clone()); }
                        if let Some(repo) = git.get("repository_url") { metadata.insert("repository".to_string(), repo.clone()); }
                    }
                }
                timeline.push(TimelineItem::SystemStatus { timestamp, status_type: "session_start".to_string(), message: "會話開始 (Session Started)".to_string() });
            }
            // 新格式: session.start
            "session.start" => {
                if let Some(p) = data.or(payload) {
                    if let Some(v) = p.get("copilotVersion") { metadata.insert("copilot_version".to_string(), v.clone()); }
                    if let Some(ctx) = p.get("context") {
                        if let Some(cwd) = ctx.get("cwd") { metadata.insert("cwd".to_string(), cwd.clone()); }
                        if let Some(branch) = ctx.get("branch") { metadata.insert("git_branch".to_string(), branch.clone()); }
                        if let Some(repo) = ctx.get("repository") { metadata.insert("repository".to_string(), repo.clone()); }
                    }
                    if let Some(model) = p.get("selectedModel").and_then(|m| m.as_str()) {
                        if model != "auto" { current_model = model.to_string(); }
                    }
                }
                timeline.push(TimelineItem::SystemStatus { timestamp, status_type: "session_start".to_string(), message: "會話開始 (Session Started)".to_string() });
            }
            "user.message" | "USER_PROMPT" => {
                let p = payload.or(data);
                if let Some(p) = p {
                    let content = p.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string();
                    let context = p.get("context").cloned();
                    timeline.push(TimelineItem::UserPrompt { timestamp, prompt: content, context, turn_no: current_turn_no });
                    has_seen_user_prompt = true;
                }
            }
            "assistant.message" | "ASSISTANT_REPLY" => {
                let p = payload.or(data);
                if let Some(p) = p {
                    let content = p.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string();
                    let reasoning = p.get("reasoning").and_then(|r| r.as_str()).map(|s| s.to_string());

                    if let Some(model) = p.get("model").and_then(|m| m.as_str()) {
                        current_model = model.to_string();
                    }

                    // 新格式: toolRequests 陣列，直接推入 ToolStep（由後續 tool.execution_complete 補結果）
                    if let Some(tool_requests) = p.get("toolRequests").and_then(|tr| tr.as_array()) {
                        for req in tool_requests {
                            let call_id = req.get("toolCallId").and_then(|i| i.as_str()).unwrap_or("").to_string();
                            let name = req.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                            let args = req.get("arguments").cloned().unwrap_or(serde_json::Value::Null);
                            let idx = timeline.len();
                            tool_calls_map.insert(call_id.clone(), idx);
                            timeline.push(TimelineItem::ToolStep {
                                timestamp: timestamp.clone(),
                                tool_name: name,
                                arguments: args,
                                env: None,
                                exit_code: None,
                                stdout: "".to_string(),
                                stderr: "".to_string(),
                                tool_call_id: Some(call_id),
                                status: "running".to_string(),
                            });
                        }
                    }

                    // 有實質回覆內容才推入 AgentReply
                    if !content.is_empty() {
                        let (tokens, model_name) = if let Some((stats, model)) = db_entries.get(&current_turn_no) {
                            current_model = model.clone();
                            (Some(stats.clone()), current_model.clone())
                        } else {
                            (None, current_model.clone())
                        };

                        timeline.push(TimelineItem::AgentReply {
                            timestamp,
                            reply: content,
                            reasoning,
                            turn_no: current_turn_no,
                            model: model_name,
                            tokens,
                            duration_ms: None,
                            reasoning_effort: None,
                        });

                        if has_seen_user_prompt {
                            current_turn_no += 1;
                            has_seen_user_prompt = false;
                        }
                    }
                }
            }
            "tool.call" | "TOOL_CALL" => {
                let p = payload.or(data);
                if let Some(p) = p {
                    let call_id = p.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
                    let name = p.get("name").and_then(|n| n.as_str()).unwrap_or("unknown").to_string();
                    let args = p.get("arguments").cloned().unwrap_or(serde_json::Value::Null);

                    let idx = timeline.len();
                    tool_calls_map.insert(call_id.clone(), idx);

                    timeline.push(TimelineItem::ToolStep {
                        timestamp,
                        tool_name: name,
                        arguments: args,
                        env: None,
                        exit_code: None,
                        stdout: "".to_string(),
                        stderr: "".to_string(),
                        tool_call_id: Some(call_id),
                        status: "running".to_string(),
                    });
                }
            }
            "tool.response" | "TOOL_RESPONSE" => {
                let p = payload.or(data);
                if let Some(p) = p {
                    let call_id = p.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
                    let stdout = p.get("stdout").and_then(|o| o.as_str()).unwrap_or("").to_string();
                    let stderr = p.get("stderr").and_then(|e| e.as_str()).unwrap_or("").to_string();
                    let exit_code = p.get("exitCode").or(p.get("exit_code")).and_then(|ec| ec.as_i64()).map(|v| v as i32);

                    if let Some(&idx) = tool_calls_map.get(&call_id) {
                        if let Some(TimelineItem::ToolStep {
                            stdout: target_stdout,
                            stderr: target_stderr,
                            exit_code: target_exit_code,
                            status,
                            ..
                        }) = timeline.get_mut(idx) {
                            *target_stdout = stdout;
                            *target_stderr = stderr;
                            *target_exit_code = exit_code;
                            *status = if exit_code.unwrap_or(0) == 0 { "success".to_string() } else { "failed".to_string() };
                        }
                    }
                }
            }
            // 新格式: tool.execution_start（若 assistant.message toolRequests 已建立此 call_id，跳過）
            "tool.execution_start" => {
                if let Some(p) = data.or(payload) {
                    let call_id = p.get("toolCallId").and_then(|i| i.as_str()).unwrap_or("").to_string();
                    if !tool_calls_map.contains_key(&call_id) {
                        let name = p.get("toolName").and_then(|n| n.as_str()).unwrap_or("").to_string();
                        let args = p.get("arguments").cloned().unwrap_or(serde_json::Value::Null);
                        let idx = timeline.len();
                        tool_calls_map.insert(call_id.clone(), idx);
                        timeline.push(TimelineItem::ToolStep {
                            timestamp,
                            tool_name: name,
                            arguments: args,
                            env: None,
                            exit_code: None,
                            stdout: "".to_string(),
                            stderr: "".to_string(),
                            tool_call_id: Some(call_id),
                            status: "running".to_string(),
                        });
                    }
                }
            }
            // 新格式: tool.execution_complete
            "tool.execution_complete" => {
                if let Some(p) = data.or(payload) {
                    let call_id = p.get("toolCallId").and_then(|i| i.as_str()).unwrap_or("").to_string();
                    let success = p.get("success").and_then(|s| s.as_bool()).unwrap_or(true);
                    // 優先取 detailedContent，其次取 content
                    let stdout = p.get("result")
                        .and_then(|r| r.get("detailedContent").or_else(|| r.get("content")))
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string();
                    let exit_code: Option<i32> = if success { Some(0) } else { Some(1) };

                    if let Some(&idx) = tool_calls_map.get(&call_id) {
                        if let Some(TimelineItem::ToolStep {
                            stdout: target_stdout,
                            exit_code: target_exit_code,
                            status,
                            ..
                        }) = timeline.get_mut(idx) {
                            *target_stdout = stdout;
                            *target_exit_code = exit_code;
                            *status = if success { "success".to_string() } else { "failed".to_string() };
                        }
                    }
                }
            }
            "session.shutdown" => {
                timeline.push(TimelineItem::SystemStatus { timestamp, status_type: "session_end".to_string(), message: "會話結束 (Session Ended)".to_string() });
            }
            _ => {}
        }
    }
    metadata.insert("selected_model".to_string(), serde_json::Value::String(current_model));
}

fn parse_codex_timeline(
    reader: BufReader<File>,
    db_entries: &HashMap<u32, (TokenStats, String)>,
    timeline: &mut Vec<TimelineItem>,
    metadata: &mut HashMap<String, serde_json::Value>,
) {
    let mut seen_turn_ids = Vec::new();
    let mut active_turn_id: Option<String> = None;
    let mut current_model = "gpt-5.3-Codex".to_string();
    let mut current_effort: Option<String> = None;
    let mut current_context: Option<serde_json::Value> = None;
    let mut tool_calls_map = HashMap::new();

    for line_res in reader.lines() {
        let line = match line_res { Ok(l) => l, Err(_) => continue };
        let event: serde_json::Value = match serde_json::from_str(&line) { Ok(v) => v, Err(_) => continue };

        let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let timestamp = event.get("timestamp").and_then(|t| t.as_str()).unwrap_or("").to_string();
        let payload = event.get("payload");

        let mut turn_id = None;
        if event_type == "turn_context" {
            if let Some(p) = payload { turn_id = p.get("turn_id").and_then(|id| id.as_str()).map(|s| s.to_string()); }
        } else if event_type == "event_msg" {
            if let Some(p) = payload { turn_id = p.get("turn_id").and_then(|id| id.as_str()).map(|s| s.to_string()); }
        } else if event_type == "response_item" {
            if let Some(meta) = event.get("metadata") { turn_id = meta.get("turn_id").and_then(|id| id.as_str()).map(|s| s.to_string()); }
            if turn_id.is_none() {
                turn_id = event.get("internal_chat_message_metadata_passthrough")
                    .and_then(|m| m.get("turn_id"))
                    .and_then(|id| id.as_str())
                    .map(|s| s.to_string());
            }
        }

        if let Some(tid) = turn_id {
            active_turn_id = Some(tid.clone());
            if !seen_turn_ids.contains(&tid) { seen_turn_ids.push(tid); }
        }

        let turn_no = active_turn_id.as_ref()
            .and_then(|tid| seen_turn_ids.iter().position(|id| id == tid))
            .map(|pos| (pos + 1) as u32)
            .unwrap_or(1);

        match event_type {
            "session_meta" => {
                if let Some(p) = payload {
                    if let Some(v) = p.get("cli_version") { metadata.insert("copilot_version".to_string(), v.clone()); }
                    if let Some(cwd) = p.get("cwd") { metadata.insert("cwd".to_string(), cwd.clone()); }
                    if let Some(git) = p.get("git") {
                        if let Some(branch) = git.get("branch") { metadata.insert("git_branch".to_string(), branch.clone()); }
                        if let Some(repo) = git.get("repository_url") { metadata.insert("repository".to_string(), repo.clone()); }
                    }
                    if let Some(nickname) = p.get("agent_nickname").or_else(|| p.get("source").and_then(|s| s.get("subagent")).and_then(|s| s.get("thread_spawn")).and_then(|t| t.get("agent_nickname"))) {
                        metadata.insert("agent_nickname".to_string(), nickname.clone());
                    }
                    if let Some(role) = p.get("agent_role").or_else(|| p.get("source").and_then(|s| s.get("subagent")).and_then(|s| s.get("thread_spawn")).and_then(|t| t.get("agent_role"))) {
                        metadata.insert("agent_role".to_string(), role.clone());
                    }
                }
                timeline.push(TimelineItem::SystemStatus { timestamp, status_type: "session_start".to_string(), message: "會話開始 (Session Started)".to_string() });
            }
            "turn_context" => {
                if let Some(p) = payload {
                    if let Some(m) = p.get("model").and_then(|v| v.as_str()) {
                        current_model = m.to_string();
                    }
                    if let Some(eff) = p.get("effort")
                        .or_else(|| p.get("collaboration_mode").and_then(|cm| cm.get("settings")).and_then(|s| s.get("reasoning_effort")))
                        .and_then(|v| v.as_str()) {
                        current_effort = Some(eff.to_string());
                    }
                    if let Some(ctx) = p.get("context") {
                        current_context = Some(ctx.clone());
                    }
                }
            }
            "compacted" => {
                timeline.push(TimelineItem::SystemStatus { timestamp, status_type: "session_compaction".to_string(), message: "會話狀態壓縮完成 (Session Compaction Completed)".to_string() });
            }
            "event_msg" => {
                if let Some(p) = payload {
                    let sub_type = p.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    match sub_type {
                        "task_started" => {
                            timeline.push(TimelineItem::SystemStatus { timestamp, status_type: "task_started".to_string(), message: "任務開始 (Task Started)".to_string() });
                        }
                        "task_complete" => {
                            timeline.push(TimelineItem::SystemStatus { timestamp, status_type: "task_complete".to_string(), message: "任務完成 (Task Completed)".to_string() });
                        }
                        "turn_aborted" => {
                            timeline.push(TimelineItem::SystemStatus { timestamp, status_type: "turn_aborted".to_string(), message: "會話中斷 (Turn Aborted)".to_string() });
                        }
                        _ => {}
                    }
                }
            }
            "tool_call" => {
                if let Some(p) = payload {
                    let call_id = p.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
                    let name = p.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                    let args = p.get("arguments").cloned().unwrap_or(serde_json::Value::Null);

                    let idx = timeline.len();
                    tool_calls_map.insert(call_id.clone(), idx);

                    timeline.push(TimelineItem::ToolStep {
                        timestamp,
                        tool_name: name,
                        arguments: args,
                        env: None,
                        exit_code: None,
                        stdout: "".to_string(),
                        stderr: "".to_string(),
                        tool_call_id: Some(call_id),
                        status: "running".to_string(),
                    });
                }
            }
            "tool_response" => {
                if let Some(p) = payload {
                    let call_id = p.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
                    let stdout = p.get("stdout").and_then(|o| o.as_str()).unwrap_or("").to_string();
                    let stderr = p.get("stderr").and_then(|e| e.as_str()).unwrap_or("").to_string();
                    let exit_code = p.get("exitCode").and_then(|ec| ec.as_i64()).map(|v| v as i32);

                    if let Some(&idx) = tool_calls_map.get(&call_id) {
                        if let Some(TimelineItem::ToolStep { stdout: target_stdout, stderr: target_stderr, exit_code: target_exit_code, status, .. }) = timeline.get_mut(idx) {
                            *target_stdout = stdout;
                            *target_stderr = stderr;
                            *target_exit_code = exit_code;
                            *status = if exit_code.unwrap_or(0) == 0 { "success".to_string() } else { "failed".to_string() };
                        }
                    }
                }
            }
            "response_item" => {
                if let Some(p) = payload {
                    let role = p.get("role").and_then(|r| r.as_str());
                    if role == Some("assistant") {
                        let mut reply = p.get("reply").and_then(|r| r.as_str()).unwrap_or("").to_string();
                        if reply.is_empty() {
                            if let Some(content_arr) = p.get("content").and_then(|c| c.as_array()) {
                                for item in content_arr {
                                    if let Some(txt) = item.get("text").and_then(|t| t.as_str()) {
                                        reply.push_str(txt);
                                    }
                                }
                            }
                        }
                        let reasoning = p.get("reasoning").and_then(|r| r.as_str()).map(|s| s.to_string());
                        
                        let (tokens, model_name) = if let Some((stats, model)) = db_entries.get(&turn_no) {
                            current_model = model.clone();
                            (Some(stats.clone()), current_model.clone())
                        } else {
                            (None, current_model.clone())
                        };

                        // Remove existing AgentReply for this turn_no to avoid duplicates
                        timeline.retain(|item| {
                            if let TimelineItem::AgentReply { turn_no: existing_turn_no, .. } = item {
                                *existing_turn_no != turn_no
                            } else {
                                true
                            }
                        });

                        timeline.push(TimelineItem::AgentReply {
                            timestamp,
                            reply,
                            reasoning,
                            turn_no,
                            model: model_name,
                            tokens,
                            duration_ms: None,
                            reasoning_effort: current_effort.clone(),
                        });
                    } else if role == Some("user") {
                        let mut prompt = p.get("prompt").and_then(|r| r.as_str()).unwrap_or("").to_string();
                        if prompt.is_empty() {
                            if let Some(content_arr) = p.get("content").and_then(|c| c.as_array()) {
                                for item in content_arr {
                                    if let Some(txt) = item.get("text").and_then(|t| t.as_str()) {
                                        prompt.push_str(txt);
                                    }
                                }
                            }
                        }
                        let context = p.get("context").cloned().or_else(|| current_context.clone());
                        timeline.push(TimelineItem::UserPrompt {
                            timestamp,
                            prompt,
                            context,
                            turn_no,
                        });
                    }
                }
            }
            _ => {}
        }
    }
    metadata.insert("selected_model".to_string(), serde_json::Value::String(current_model));
    if let Some(eff) = current_effort {
        metadata.insert("reasoning_effort".to_string(), serde_json::Value::String(eff));
    }
}

/// API 5: 獲取可用的有使用記錄月份
async fn get_available_months(Path(assistant): Path<String>) -> impl IntoResponse {
    let _ = tokio::task::spawn_blocking(|| {
        if let Ok(conn) = db::get_db_conn() {
            let _ = db::sync_usage_logs(&conn);
        }
    }).await;

    let res: Result<Vec<String>, String> = tokio::task::spawn_blocking(move || {
        let conn = db::get_db_conn()?;
        let mut months = Vec::new();
        if assistant == "all" {
            let mut stmt = conn.prepare("SELECT DISTINCT substr(date, 1, 7) FROM usage_entries ORDER BY date DESC").map_err(|e| e.to_string())?;
            let month_iter = stmt.query_map([], |row| row.get::<_, String>(0)).map_err(|e| e.to_string())?;
            for m in month_iter {
                months.push(m.map_err(|e| e.to_string())?);
            }
        } else {
            let assistants: Vec<&str> = assistant.split(',').collect();
            let mut placeholders = Vec::new();
            let mut params_vec = Vec::new();
            for a in assistants {
                placeholders.push("?");
                params_vec.push(rusqlite::types::Value::Text(a.to_string()));
            }
            let query = format!(
                "SELECT DISTINCT substr(date, 1, 7) FROM usage_entries WHERE assistant_type IN ({}) ORDER BY date DESC",
                placeholders.join(",")
            );
            let mut stmt = conn.prepare(&query).map_err(|e| e.to_string())?;
            let month_iter = stmt.query_map(rusqlite::params_from_iter(params_vec), |row| row.get::<_, String>(0)).map_err(|e| e.to_string())?;
            for m in month_iter {
                months.push(m.map_err(|e| e.to_string())?);
            }
        }
        Ok(months)
    }).await.unwrap_or_else(|_| Err("執行緒執行失敗".to_string()));

    match res {
        Ok(month_list) => Json(MonthListResponse { months: month_list }).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e }))).into_response(),
    }
}

#[derive(Serialize)]
struct MonthlyDailyBreakdown {
    date: String,
    total_tokens: u64,
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_cache_read_tokens: u64,
    total_reasoning_tokens: u64,
    sessions_count: usize,
    cost_usd: f64,
}

#[derive(Serialize)]
struct MonthlyProjectSummary {
    cwd: String,
    sessions_count: usize,
    total_tokens: u64,
    cost_usd: f64,
}

#[derive(Serialize)]
struct MonthlyModelSummary {
    model: String,
    sessions_count: usize,
    total_tokens: u64,
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_cache_read_tokens: u64,
    cost_usd: f64,
}

#[derive(Serialize, Default, Clone)]
struct AgentBreakdown {
    total_tokens: u64,
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_cache_read_tokens: u64,
    total_reasoning_tokens: u64,
    total_cost_usd: f64,
    total_sessions: usize,
}

#[derive(Serialize)]
struct MonthlyDetailsResponse {
    year_month: String,
    summary: DaySummary,
    daily_breakdown: Vec<MonthlyDailyBreakdown>,
    projects: Vec<MonthlyProjectSummary>,
    models: Vec<MonthlyModelSummary>,
    agent_breakdown: HashMap<String, AgentBreakdown>,
}

/// API 6: 獲取指定月份的統計摘要數據
async fn get_monthly_details(Path((assistant, year_month)): Path<(String, String)>) -> impl IntoResponse {
    let _ = tokio::task::spawn_blocking(|| {
        if let Ok(conn) = db::get_db_conn() {
            let _ = db::sync_usage_logs(&conn);
        }
    }).await;

    let assistant_clone = assistant.clone();
    let query_month = format!("{}-%", year_month);

    let entries_res: Result<Vec<(UsageEntry, String)>, String> = tokio::task::spawn_blocking(move || {
        let conn = db::get_db_conn()?;
        let mut query = "SELECT 
                timestamp, session_id, session_name, transcript_path, cwd, version, turn_no, model, model_id,
                tokens_input, tokens_output, tokens_cache_read, tokens_reasoning, tokens_total,
                delta_input, delta_output, delta_cache_read, delta_reasoning, delta_total,
                duration_ms, premium_requests, parent_session_id, agent_nickname, agent_role, assistant_type, reasoning_effort
             FROM usage_entries WHERE date LIKE ?".to_string();
        let mut params_vec = Vec::new();
        params_vec.push(rusqlite::types::Value::Text(query_month));

        if assistant_clone != "all" {
            let assistants: Vec<&str> = assistant_clone.split(',').collect();
            let mut placeholders = Vec::new();
            for a in assistants {
                placeholders.push("?");
                params_vec.push(rusqlite::types::Value::Text(a.to_string()));
            }
            query.push_str(&format!(" AND assistant_type IN ({})", placeholders.join(",")));
        }
        query.push_str(" ORDER BY timestamp ASC");

        let mut stmt = conn.prepare(&query).map_err(|e| e.to_string())?;
        let mut rows = stmt.query(rusqlite::params_from_iter(params_vec)).map_err(|e| e.to_string())?;

        let mut entries = Vec::new();
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let ast_type = row.get::<_, String>(24).map_err(|e| e.to_string())?;
            let tokens_input: Option<u64> = row.get::<_, Option<i64>>(9).map_err(|e| e.to_string())?.map(|v| v as u64);
            let tokens_output: Option<u64> = row.get::<_, Option<i64>>(10).map_err(|e| e.to_string())?.map(|v| v as u64);
            let tokens_cache_read: Option<u64> = row.get::<_, Option<i64>>(11).map_err(|e| e.to_string())?.map(|v| v as u64);
            let tokens_reasoning: Option<u64> = row.get::<_, Option<i64>>(12).map_err(|e| e.to_string())?.map(|v| v as u64);
            let tokens_total: Option<u64> = row.get::<_, Option<i64>>(13).map_err(|e| e.to_string())?.map(|v| v as u64);

            let tokens = if let (Some(input), Some(output), Some(total)) = (tokens_input, tokens_output, tokens_total) {
                Some(TokenStats { input, output, cache_read: tokens_cache_read, cache_write: None, reasoning: tokens_reasoning, total })
            } else {
                None
            };

            let delta_input: Option<u64> = row.get::<_, Option<i64>>(14).map_err(|e| e.to_string())?.map(|v| v as u64);
            let delta_output: Option<u64> = row.get::<_, Option<i64>>(15).map_err(|e| e.to_string())?.map(|v| v as u64);
            let delta_cache_read: Option<u64> = row.get::<_, Option<i64>>(16).map_err(|e| e.to_string())?.map(|v| v as u64);
            let delta_reasoning: Option<u64> = row.get::<_, Option<i64>>(17).map_err(|e| e.to_string())?.map(|v| v as u64);
            let delta_total: Option<u64> = row.get::<_, Option<i64>>(18).map_err(|e| e.to_string())?.map(|v| v as u64);

            let delta_tokens = if let (Some(input), Some(output), Some(total)) = (delta_input, delta_output, delta_total) {
                Some(TokenStats { input, output, cache_read: delta_cache_read, cache_write: None, reasoning: delta_reasoning, total })
            } else {
                None
            };

            let duration_ms: Option<f64> = row.get::<_, Option<i64>>(19).map_err(|e| e.to_string())?.map(|v| v as f64);
            let premium_requests: Option<f64> = row.get::<_, Option<i64>>(20).map_err(|e| e.to_string())?.map(|v| v as f64);

            let cost = if duration_ms.is_some() || premium_requests.is_some() {
                Some(CostStats { total_api_duration_ms: duration_ms, total_duration_ms: None, total_premium_requests: premium_requests })
            } else {
                None
            };

            entries.push((UsageEntry {
                timestamp: row.get(0).map_err(|e| e.to_string())?,
                session_id: row.get(1).map_err(|e| e.to_string())?,
                session_name: row.get(2).ok(),
                transcript_path: row.get(3).ok(),
                cwd: row.get(4).ok(),
                version: row.get(5).ok(),
                turn_no: row.get::<_, i64>(6).map_err(|e| e.to_string())? as u32,
                model: row.get(7).ok(),
                model_id: row.get(8).ok(),
                tokens,
                delta_tokens,
                context: None,
                cost,
                parent_session_id: row.get(21).ok(),
                agent_nickname: row.get(22).ok(),
                agent_role: row.get(23).ok(),
                reasoning_effort: row.get(25).ok(),
            }, ast_type));
        }
        Ok(entries)
    }).await.unwrap_or_else(|_| Err("執行緒執行失敗".to_string()));

    let entries_with_type = match entries_res {
        Ok(e) => e,
        Err(err) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": err }))).into_response(),
    };

    if entries_with_type.is_empty() {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "找不到該月份的使用量資料。" }))).into_response();
    }

    let pricing_rules = load_pricing_rules();
    let mut daily_map: HashMap<String, Vec<(UsageEntry, String)>> = HashMap::new();
    let mut sessions_map: HashMap<String, (Vec<UsageEntry>, String)> = HashMap::new();
    
    for (e, ast_type) in &entries_with_type {
        if e.timestamp.len() >= 10 {
            let d = e.timestamp[0..10].to_string();
            daily_map.entry(d).or_default().push((e.clone(), ast_type.clone()));
        }
        let (list, _) = sessions_map.entry(e.session_id.clone()).or_insert_with(|| (Vec::new(), ast_type.clone()));
        list.push(e.clone());
    }

    let mut daily_breakdown = Vec::new();
    let mut monthly_summary = DaySummary::default();
    monthly_summary.total_sessions = sessions_map.len();

    let mut session_last_entries: HashMap<String, UsageEntry> = HashMap::new();
    for (e, _) in &entries_with_type {
        let sid = e.session_id.clone();
        let last_e = session_last_entries.entry(sid).or_insert_with(|| e.clone());
        if e.turn_no > last_e.turn_no {
            *last_e = e.clone();
        }
    }

    // 計算每日彙整與月彙整
    let mut sorted_dates: Vec<String> = daily_map.keys().cloned().collect();
    sorted_dates.sort();

    for date_str in sorted_dates {
        let day_entries_with_type = daily_map.get(&date_str).unwrap();
        let mut day_tokens = 0;
        let mut day_input = 0;
        let mut day_output = 0;
        let mut day_reasoning = 0;
        let mut day_cache_read = 0;
        let mut day_cost_usd = 0.0;
        let mut day_sessions = HashSet::new();

        let mut day_sessions_map: HashMap<String, Vec<UsageEntry>> = HashMap::new();
        for (e, _) in day_entries_with_type {
            day_sessions.insert(e.session_id.clone());
            day_sessions_map.entry(e.session_id.clone()).or_default().push(e.clone());
        }

        for (sid, s_entries) in &day_sessions_map {
            let s_tokens = s_entries.iter().map(|e| e.delta_tokens.as_ref().map(|t| t.total).unwrap_or(0)).sum::<u64>();
            let s_input = s_entries.iter().map(|e| e.delta_tokens.as_ref().map(|t| t.input).unwrap_or(0)).sum::<u64>();
            let s_output = s_entries.iter().map(|e| e.delta_tokens.as_ref().map(|t| t.output).unwrap_or(0)).sum::<u64>();
            let s_cache = s_entries.iter().map(|e| e.delta_tokens.as_ref().and_then(|t| t.cache_read).unwrap_or(0)).sum::<u64>();
            let s_reasoning = s_entries.iter().map(|e| e.delta_tokens.as_ref().and_then(|t| t.reasoning).unwrap_or(0)).sum::<u64>();

            let last_entry = session_last_entries.get(sid).cloned().unwrap_or_else(|| s_entries[0].clone());
            let final_input = if s_tokens > 0 { s_input } else { last_entry.tokens.as_ref().map(|t| t.input).unwrap_or(0) };
            let final_output = if s_tokens > 0 { s_output } else { last_entry.tokens.as_ref().map(|t| t.output).unwrap_or(0) };
            let final_cache = if s_tokens > 0 { s_cache } else { last_entry.tokens.as_ref().and_then(|t| t.cache_read).unwrap_or(0) };
            let final_reasoning = if s_tokens > 0 { s_reasoning } else { last_entry.tokens.as_ref().and_then(|t| t.reasoning).unwrap_or(0) };
            let final_total = if s_tokens > 0 { s_tokens } else { last_entry.tokens.as_ref().map(|t| t.total).unwrap_or(0) };

            let cost_usd = calculate_cost(
                &pricing_rules,
                &last_entry.model.clone().unwrap_or_else(|| "Unknown Model".to_string()),
                final_input,
                final_output,
                final_cache,
            );

            day_tokens += final_total;
            day_input += final_input;
            day_output += final_output;
            day_cache_read += final_cache;
            day_reasoning += final_reasoning;
            day_cost_usd += cost_usd;
        }

        monthly_summary.total_tokens += day_tokens;
        monthly_summary.total_input_tokens += day_input;
        monthly_summary.total_output_tokens += day_output;
        monthly_summary.total_cache_read_tokens += day_cache_read;
        monthly_summary.total_reasoning_tokens += day_reasoning;
        monthly_summary.total_cost_usd += day_cost_usd;

        daily_breakdown.push(MonthlyDailyBreakdown {
            date: date_str,
            total_tokens: day_tokens,
            total_input_tokens: day_input,
            total_output_tokens: day_output,
            total_cache_read_tokens: day_cache_read,
            total_reasoning_tokens: day_reasoning,
            sessions_count: day_sessions.len(),
            cost_usd: day_cost_usd,
        });
    }

    // 按專案統計 (CWD)
    let mut project_map_stats: HashMap<String, (usize, u64, f64)> = HashMap::new();
    // 按模型統計 (Model)
    let mut model_map_stats: HashMap<String, (usize, u64, u64, u64, u64, f64)> = HashMap::new();
    // 按 Agent 類型統計
    let mut agent_map_stats: HashMap<String, AgentBreakdown> = HashMap::new();

    for (session_id, (s_entries, ast_type)) in &sessions_map {
        let last_entry = session_last_entries.get(session_id).cloned().unwrap_or_else(|| s_entries[0].clone());
        
        let s_tokens = s_entries.iter().map(|e| e.delta_tokens.as_ref().map(|t| t.total).unwrap_or(0)).sum::<u64>();
        let s_input = s_entries.iter().map(|e| e.delta_tokens.as_ref().map(|t| t.input).unwrap_or(0)).sum::<u64>();
        let s_output = s_entries.iter().map(|e| e.delta_tokens.as_ref().map(|t| t.output).unwrap_or(0)).sum::<u64>();
        let s_cache = s_entries.iter().map(|e| e.delta_tokens.as_ref().and_then(|t| t.cache_read).unwrap_or(0)).sum::<u64>();
        let s_reasoning = s_entries.iter().map(|e| e.delta_tokens.as_ref().and_then(|t| t.reasoning).unwrap_or(0)).sum::<u64>();

        let final_input = if s_tokens > 0 { s_input } else { last_entry.tokens.as_ref().map(|t| t.input).unwrap_or(0) };
        let final_output = if s_tokens > 0 { s_output } else { last_entry.tokens.as_ref().map(|t| t.output).unwrap_or(0) };
        let final_cache = if s_tokens > 0 { s_cache } else { last_entry.tokens.as_ref().and_then(|t| t.cache_read).unwrap_or(0) };
        let final_reasoning = if s_tokens > 0 { s_reasoning } else { last_entry.tokens.as_ref().and_then(|t| t.reasoning).unwrap_or(0) };
        let final_total = if s_tokens > 0 { s_tokens } else { last_entry.tokens.as_ref().map(|t| t.total).unwrap_or(0) };

        let cost_usd = calculate_cost(
            &pricing_rules,
            &last_entry.model.clone().unwrap_or_else(|| "Unknown Model".to_string()),
            final_input,
            final_output,
            final_cache,
        );

        let cwd = last_entry.cwd.unwrap_or_else(|| "Unknown CWD".to_string());
        let project_stat = project_map_stats.entry(cwd).or_insert((0, 0, 0.0));
        project_stat.0 += 1;
        project_stat.1 += final_total;
        project_stat.2 += cost_usd;

        let model = last_entry.model.unwrap_or_else(|| "Unknown Model".to_string());
        let model_stat = model_map_stats.entry(model).or_insert((0, 0, 0, 0, 0, 0.0));
        model_stat.0 += 1;
        model_stat.1 += final_total;
        model_stat.2 += final_input;
        model_stat.3 += final_output;
        model_stat.4 += final_cache;
        model_stat.5 += cost_usd;

        let agent_stat = agent_map_stats.entry(ast_type.clone()).or_default();
        agent_stat.total_tokens += final_total;
        agent_stat.total_input_tokens += final_input;
        agent_stat.total_output_tokens += final_output;
        agent_stat.total_cache_read_tokens += final_cache;
        agent_stat.total_reasoning_tokens += final_reasoning;
        agent_stat.total_cost_usd += cost_usd;
        agent_stat.total_sessions += 1;
    }

    let mut project_summaries = Vec::new();
    for (cwd, (sessions_count, total_tokens, cost_usd)) in project_map_stats {
        project_summaries.push(MonthlyProjectSummary { cwd, sessions_count, total_tokens, cost_usd });
    }
    project_summaries.sort_by(|a, b| b.total_tokens.cmp(&a.total_tokens));

    let mut model_summaries = Vec::new();
    for (model, (sessions_count, total_tokens, total_input_tokens, total_output_tokens, total_cache_read_tokens, cost_usd)) in model_map_stats {
        model_summaries.push(MonthlyModelSummary { model, sessions_count, total_tokens, total_input_tokens, total_output_tokens, total_cache_read_tokens, cost_usd });
    }
    model_summaries.sort_by(|a, b| b.total_tokens.cmp(&a.total_tokens));

    Json(MonthlyDetailsResponse {
        year_month,
        summary: monthly_summary,
        daily_breakdown,
        projects: project_summaries,
        models: model_summaries,
        agent_breakdown: agent_map_stats,
    }).into_response()
}

/// API 7: 獲取模型價格清單 ( pricing.csv 資訊)
async fn get_pricing(Path(_assistant): Path<String>) -> impl IntoResponse {
    let mut entries = Vec::new();
    let file_path = PathBuf::from("pricing.csv");
    if let Ok(file) = File::open(&file_path) {
        let reader = BufReader::new(file);
        let mut lines = reader.lines();
        if let Some(Ok(_header)) = lines.next() {
            for line in lines.flatten() {
                let parts: Vec<&str> = line.split(',').collect();
                if parts.len() >= 6 {
                    let input_price = parts[3].trim().parse::<f64>().unwrap_or(0.0);
                    let cache_input_price = parts[4].trim().parse::<f64>().unwrap_or(0.0);
                    let output_price = parts[5].trim().parse::<f64>().unwrap_or(0.0);
                    let batch_api_price = if parts.len() >= 7 { parts[6].trim().to_string() } else { "N/A".to_string() };
                    entries.push(PricingEntry {
                        model_name: parts[0].trim().to_string(),
                        deployment_type: parts[1].trim().to_string(),
                        unit: parts[2].trim().to_string(),
                        input_price,
                        cache_input_price,
                        output_price,
                        batch_api_price,
                    });
                }
            }
        }
    }
    if entries.is_empty() {
        entries = vec![
            PricingEntry { model_name: "Gemini 3.5 Flash".to_string(), deployment_type: "Google AI".to_string(), unit: "1M Tokens".to_string(), input_price: 0.075, cache_input_price: 0.01875, output_price: 0.30, batch_api_price: "N/A".to_string() },
            PricingEntry { model_name: "Gemini 3.5 Pro".to_string(), deployment_type: "Google AI".to_string(), unit: "1M Tokens".to_string(), input_price: 1.25, cache_input_price: 0.3125, output_price: 5.00, batch_api_price: "N/A".to_string() },
        ];
    }
    Json(entries)
}

/// API 8: 手動觸發日誌增量同步
async fn trigger_manual_sync(Path(_assistant): Path<String>) -> impl IntoResponse {
    let sync_res = tokio::task::spawn_blocking(|| {
        if let Ok(conn) = db::get_db_conn() {
            db::sync_usage_logs(&conn)
        } else {
            Err("無法連接至 SQLite 資料庫".to_string())
        }
    }).await;

    match sync_res {
        Ok(Ok(_)) => (StatusCode::OK, Json(serde_json::json!({ "status": "success", "message": "手動增量同步已成功完成！" }))).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "status": "error", "message": format!("同步失敗: {}", e) }))).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "status": "error", "message": "執行緒執行失敗" }))).into_response(),
    }
}
