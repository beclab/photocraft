//! HTTP service for agents: upload an image, run PhotoCraft engine commands on it, and get the
//! result back. Every request gets its own [`Headless`] session confined to a temporary
//! directory, so concurrent callers never see each other's documents or files.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::multipart::MultipartError;
use axum::extract::{DefaultBodyLimit, Multipart, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use photocraft_automation::{AuthorizedWorkspace, AutomationError, Headless};
use serde_json::{Value, json};
use tokio::sync::Semaphore;

/// Output formats `/v1/process` writes, with the MIME type it answers with.
const OUTPUT_FORMATS: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("webp", "image/webp"),
    ("gif", "image/gif"),
    ("bmp", "image/bmp"),
    ("tif", "image/tiff"),
    ("tiff", "image/tiff"),
    ("tga", "image/x-tga"),
    ("qoi", "image/qoi"),
    ("exr", "image/x-exr"),
    ("psd", "image/vnd.adobe.photoshop"),
    ("psb", "image/vnd.adobe.photoshop"),
];

const MAX_TEXT_FIELDS: usize = 16;

#[derive(Clone, Debug)]
pub struct Config {
    pub max_upload_bytes: usize,
    pub max_jobs: usize,
    pub timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self { max_upload_bytes: 256 << 20, max_jobs: 2, timeout: Duration::from_secs(300) }
    }
}

#[derive(Clone)]
struct AppState {
    jobs: Arc<Semaphore>,
    timeout: Duration,
}

pub fn router(cfg: &Config) -> Router {
    let state = AppState { jobs: Arc::new(Semaphore::new(cfg.max_jobs.max(1))), timeout: cfg.timeout };
    Router::new()
        .route("/healthz", get(|| async { "ok\n" }))
        .route("/v1/commands", get(commands))
        .route("/v1/process", post(process))
        .route("/v1/inspect", post(inspect))
        .layer(DefaultBodyLimit::max(cfg.max_upload_bytes))
        .with_state(state)
}

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    body: Value,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self { status, body: json!({ "error": message.into() }) }
    }

    fn bad(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

impl From<AutomationError> for ApiError {
    fn from(e: AutomationError) -> Self {
        let status = match e {
            AutomationError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AutomationError::Engine(_) | AutomationError::Import(_) | AutomationError::Format(_) => StatusCode::UNPROCESSABLE_ENTITY,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self::new(status, e.to_string())
    }
}

fn multipart_error(e: MultipartError) -> ApiError {
    ApiError::new(e.status(), e.body_text())
}

/// A lowercase extension of 1–8 ASCII letters or digits, or `None`.
fn sanitize_ext(raw: &str) -> Option<String> {
    let ext = raw.trim().trim_start_matches('.').to_ascii_lowercase();
    (!ext.is_empty() && ext.len() <= 8 && ext.bytes().all(|b| b.is_ascii_alphanumeric())).then_some(ext)
}

fn ext_of_file_name(name: &str) -> Option<String> {
    name.rsplit_once('.').and_then(|(_, ext)| sanitize_ext(ext))
}

fn output_mime(ext: &str) -> Option<&'static str> {
    OUTPUT_FORMATS.iter().find(|(e, _)| *e == ext).map(|(_, mime)| *mime)
}

struct Upload {
    bytes: Vec<u8>,
    ext: String,
    fields: HashMap<String, String>,
}

async fn read_upload(mut multipart: Multipart) -> Result<Upload, ApiError> {
    let mut file: Option<(Vec<u8>, Option<String>)> = None;
    let mut fields = HashMap::new();
    while let Some(field) = multipart.next_field().await.map_err(multipart_error)? {
        let name = field.name().unwrap_or_default().to_string();
        if name == "file" {
            let ext = field.file_name().and_then(ext_of_file_name);
            let bytes = field.bytes().await.map_err(multipart_error)?;
            file = Some((bytes.to_vec(), ext));
        } else if !name.is_empty() {
            if fields.len() >= MAX_TEXT_FIELDS {
                return Err(ApiError::bad(format!("too many form fields (maximum {MAX_TEXT_FIELDS})")));
            }
            let text = field.text().await.map_err(multipart_error)?;
            fields.insert(name, text);
        }
    }
    let (bytes, file_ext) = file.ok_or_else(|| ApiError::bad("missing the `file` form field"))?;
    if bytes.is_empty() {
        return Err(ApiError::bad("the uploaded `file` is empty"));
    }
    let ext = match fields.get("inputFormat") {
        Some(raw) => sanitize_ext(raw).ok_or_else(|| ApiError::bad(format!("`inputFormat` `{raw}` is not a file extension")))?,
        None => file_ext.ok_or_else(|| ApiError::bad("cannot tell the input format: name the file with its extension or set `inputFormat`"))?,
    };
    Ok(Upload { bytes, ext, fields })
}

/// `steps` is a JSON array of `{command, params?}`. Every step runs to completion: a `wait`
/// flag is dropped, since a background job would outlive the request's session.
fn parse_steps(text: Option<&String>) -> Result<Vec<Value>, ApiError> {
    let Some(text) = text else { return Ok(Vec::new()) };
    let value: Value = serde_json::from_str(text).map_err(|e| ApiError::bad(format!("`steps` is not valid JSON: {e}")))?;
    let steps = value.as_array().ok_or_else(|| ApiError::bad("`steps` must be a JSON array of {\"command\", \"params\"} objects"))?;
    steps
        .iter()
        .enumerate()
        .map(|(i, step)| {
            let command = step.get("command").and_then(Value::as_str).ok_or_else(|| ApiError::bad(format!("step {i} needs a string `command`")))?;
            let params = step.get("params").cloned().unwrap_or(Value::Null);
            Ok(json!({ "command": command, "params": params }))
        })
        .collect()
}

async fn run_job<T, F>(state: &AppState, job: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ApiError> + Send + 'static,
{
    let jobs = Arc::clone(&state.jobs);
    let work = async move {
        let permit = jobs.acquire_owned().await.map_err(|_| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "the server is shutting down"))?;
        // The permit moves into the blocking task, so a timed-out request keeps its slot until
        // the engine call it started has actually returned.
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            job()
        })
        .await
        .map_err(|e| ApiError::internal(if e.is_panic() { "the engine failed on this input" } else { "the job was cancelled" }))?
    };
    tokio::time::timeout(state.timeout, work)
        .await
        .map_err(|_| ApiError::new(StatusCode::GATEWAY_TIMEOUT, format!("the job did not finish within {} s", state.timeout.as_secs())))?
}

/// Write the upload into a fresh directory and open it in a session confined to that directory.
fn open_upload(bytes: &[u8], ext: &str) -> Result<(tempfile::TempDir, Headless), ApiError> {
    let dir = tempfile::tempdir().map_err(|e| ApiError::internal(format!("cannot create a work directory: {e}")))?;
    let input = format!("input.{ext}");
    std::fs::write(dir.path().join(&input), bytes).map_err(|e| ApiError::internal(format!("cannot store the upload: {e}")))?;
    let workspace = AuthorizedWorkspace::new(Some(dir.path()), Some(dir.path()))?;
    let mut headless = Headless::with_workspace(workspace);
    headless.handle("doc.open", json!({ "path": input }))?;
    Ok((dir, headless))
}

fn process_blocking(upload: &Upload, steps: &[Value], out_ext: &str, quality: Option<u8>) -> Result<Vec<u8>, ApiError> {
    let (dir, mut headless) = open_upload(&upload.bytes, &upload.ext)?;
    if !steps.is_empty() {
        let reply = headless.handle("batch", json!({ "steps": steps, "stopOnError": true }))?;
        if reply.get("failed").and_then(Value::as_u64).unwrap_or(0) > 0 {
            return Err(ApiError { status: StatusCode::UNPROCESSABLE_ENTITY, body: json!({ "error": "a step failed", "batch": reply }) });
        }
    }
    let output = format!("output.{out_ext}");
    let mut save = serde_json::Map::new();
    save.insert("path".into(), json!(output));
    if let Some(q) = quality {
        save.insert("quality".into(), json!(q));
    }
    headless.handle("doc.save", Value::Object(save))?;
    std::fs::read(dir.path().join(&output)).map_err(|e| ApiError::internal(format!("cannot read the result: {e}")))
}

async fn process(State(state): State<AppState>, multipart: Multipart) -> Result<Response, ApiError> {
    let upload = read_upload(multipart).await?;
    let steps = parse_steps(upload.fields.get("steps"))?;
    let out_ext = match upload.fields.get("format") {
        Some(raw) => sanitize_ext(raw).ok_or_else(|| ApiError::bad(format!("`format` `{raw}` is not a file extension")))?,
        None => "png".to_string(),
    };
    let mime = output_mime(&out_ext).ok_or_else(|| {
        let known: Vec<&str> = OUTPUT_FORMATS.iter().map(|(e, _)| *e).collect();
        ApiError::bad(format!("unsupported output format `{out_ext}`; use one of {}", known.join(", ")))
    })?;
    let quality = match upload.fields.get("quality") {
        None => None,
        Some(raw) => {
            Some(raw.trim().parse::<u8>().ok().filter(|q| (1..=100).contains(q)).ok_or_else(|| ApiError::bad("`quality` must be an integer from 1 to 100"))?)
        }
    };
    let ext = out_ext.clone();
    let bytes = run_job(&state, move || process_blocking(&upload, &steps, &ext, quality)).await?;
    let disposition = format!("attachment; filename=\"output.{out_ext}\"");
    Ok((StatusCode::OK, [(header::CONTENT_TYPE, mime.to_string()), (header::CONTENT_DISPOSITION, disposition)], bytes).into_response())
}

async fn inspect(State(state): State<AppState>, multipart: Multipart) -> Result<Json<Value>, ApiError> {
    let upload = read_upload(multipart).await?;
    let report = run_job(&state, move || {
        let (_dir, mut headless) = open_upload(&upload.bytes, &upload.ext)?;
        Ok(headless.handle("doc.inspect", json!({}))?)
    })
    .await?;
    Ok(Json(report))
}

async fn commands(State(state): State<AppState>, Query(query): Query<HashMap<String, String>>) -> Result<Json<Value>, ApiError> {
    let params = match query.get("filter") {
        Some(filter) => json!({ "filter": filter }),
        None => json!({}),
    };
    let list = run_job(&state, move || Ok(Headless::new().handle("engine.commands", params)?)).await?;
    Ok(Json(list))
}
