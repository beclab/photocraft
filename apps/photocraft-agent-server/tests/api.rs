use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use photocraft_agent_server::{Config, router};
use photocraft_automation::{AuthorizedWorkspace, Headless};
use serde_json::{Value, json};
use tower::ServiceExt;

const BOUNDARY: &str = "photocraft-test-boundary";

enum Part<'a> {
    File(&'a str, &'a [u8]),
    Text(&'a str, &'a str),
}

fn multipart(parts: &[Part]) -> Vec<u8> {
    let mut body = Vec::new();
    for part in parts {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        match part {
            Part::File(name, bytes) => {
                body.extend_from_slice(format!("Content-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n").as_bytes());
                body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
                body.extend_from_slice(bytes);
            }
            Part::Text(name, value) => {
                body.extend_from_slice(format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes());
                body.extend_from_slice(value.as_bytes());
            }
        }
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

async fn post(app: &Router, path: &str, parts: &[Part<'_>]) -> (StatusCode, Option<String>, Vec<u8>) {
    let request =
        Request::post(path).header(header::CONTENT_TYPE, format!("multipart/form-data; boundary={BOUNDARY}")).body(Body::from(multipart(parts))).unwrap();
    send(app, request).await
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, Option<String>, Vec<u8>) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let mime = response.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let bytes = response.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, mime, bytes)
}

fn json_of(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap()
}

/// A 16x8 white PNG written by the engine itself.
fn sample_png() -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Headless::with_workspace(AuthorizedWorkspace::new(Some(dir.path()), Some(dir.path())).unwrap());
    h.handle("doc.new", json!({"width": 16, "height": 8, "background": "white"})).unwrap();
    h.handle("doc.save", json!({"path": "sample.png"})).unwrap();
    std::fs::read(dir.path().join("sample.png")).unwrap()
}

fn app() -> Router {
    router(&Config::default())
}

#[tokio::test]
async fn healthz_answers() {
    let (status, _, body) = send(&app(), Request::get("/healthz").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"ok\n");
}

#[tokio::test]
async fn commands_can_be_filtered() {
    let (status, _, body) = send(&app(), Request::get("/v1/commands?filter=gaussian").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    let list = json_of(&body);
    let ids: Vec<&str> = list.as_array().unwrap().iter().filter_map(|c| c["id"].as_str()).collect();
    assert!(ids.contains(&"filter.blur.gaussianBlur"), "{ids:?}");
}

#[tokio::test]
async fn process_runs_steps_and_returns_the_image() {
    let input = sample_png();
    let steps = r#"[{"command":"image.adjustments.invert"},{"command":"filter.blur.gaussianBlur","params":{"radius":1}}]"#;
    let (status, mime, out) = post(&app(), "/v1/process", &[Part::File("in.png", &input), Part::Text("steps", steps)]).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&out));
    assert_eq!(mime.as_deref(), Some("image/png"));
    assert!(out.starts_with(b"\x89PNG"));
    assert_ne!(out, input, "inverting white must change the pixels");

    let (status, _, report) = post(&app(), "/v1/inspect", &[Part::File("out.png", &out)]).await;
    assert_eq!(status, StatusCode::OK);
    let report = json_of(&report);
    assert_eq!(report["width"], 16, "{report}");
    assert_eq!(report["height"], 8, "{report}");
}

#[tokio::test]
async fn process_converts_without_steps() {
    let input = sample_png();
    let (status, mime, out) = post(&app(), "/v1/process", &[Part::File("in.png", &input), Part::Text("format", "jpg"), Part::Text("quality", "80")]).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&out));
    assert_eq!(mime.as_deref(), Some("image/jpeg"));
    assert!(out.starts_with(&[0xFF, 0xD8]));
}

#[tokio::test]
async fn input_format_overrides_a_missing_extension() {
    let input = sample_png();
    let (status, _, _) = post(&app(), "/v1/process", &[Part::File("upload", &input), Part::Text("inputFormat", "png")]).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, body) = post(&app(), "/v1/process", &[Part::File("upload", &input)]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(json_of(&body)["error"].as_str().unwrap().contains("inputFormat"));
}

#[tokio::test]
async fn bad_requests_are_errors_not_crashes() {
    let input = sample_png();
    let app = app();
    let cases: Vec<(Vec<Part>, StatusCode)> = vec![
        (vec![Part::Text("steps", "[]")], StatusCode::BAD_REQUEST),
        (vec![Part::File("in.png", b"")], StatusCode::BAD_REQUEST),
        (vec![Part::File("in.png", &input), Part::Text("steps", "{\"command\":\"x\"}")], StatusCode::BAD_REQUEST),
        (vec![Part::File("in.png", &input), Part::Text("steps", "not json")], StatusCode::BAD_REQUEST),
        (vec![Part::File("in.png", &input), Part::Text("steps", "[{\"params\":{}}]")], StatusCode::BAD_REQUEST),
        (vec![Part::File("in.png", &input), Part::Text("format", "pcraft")], StatusCode::BAD_REQUEST),
        (vec![Part::File("in.png", &input), Part::Text("format", "../x")], StatusCode::BAD_REQUEST),
        (vec![Part::File("in.png", &input), Part::Text("quality", "0")], StatusCode::BAD_REQUEST),
        (vec![Part::File("in.png", &input), Part::Text("inputFormat", "p/ng")], StatusCode::BAD_REQUEST),
        (vec![Part::File("in.png", b"definitely not a png")], StatusCode::UNPROCESSABLE_ENTITY),
        (vec![Part::File("in.zzz", b"\x00\x01\x02 not an image")], StatusCode::UNPROCESSABLE_ENTITY),
    ];
    for (parts, want) in cases {
        let (status, mime, body) = post(&app, "/v1/process", &parts).await;
        assert_eq!(status, want, "{}", String::from_utf8_lossy(&body));
        assert_eq!(mime.as_deref(), Some("application/json"));
        assert!(json_of(&body)["error"].is_string());
    }
}

#[tokio::test]
async fn a_failing_step_reports_the_batch() {
    let input = sample_png();
    let steps = r#"[{"command":"image.adjustments.invert"},{"command":"no.such.command"}]"#;
    let (status, _, body) = post(&app(), "/v1/process", &[Part::File("in.png", &input), Part::Text("steps", steps)]).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let body = json_of(&body);
    assert_eq!(body["batch"]["failed"], 1, "{body}");
    assert_eq!(body["batch"]["results"][0]["ok"], true, "{body}");
}

#[tokio::test]
async fn steps_cannot_reach_outside_the_request_directory() {
    let input = sample_png();
    let steps = r#"[{"command":"file.open","params":{"path":"/etc/passwd"}}]"#;
    let (status, _, body) = post(&app(), "/v1/process", &[Part::File("in.png", &input), Part::Text("steps", steps)]).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{}", String::from_utf8_lossy(&body));
}

#[tokio::test]
async fn oversized_uploads_are_refused() {
    let app = router(&Config { max_upload_bytes: 1024, ..Config::default() });
    let big = vec![0u8; 8 * 1024];
    let (status, _, _) = post(&app, "/v1/process", &[Part::File("in.png", &big)]).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}
