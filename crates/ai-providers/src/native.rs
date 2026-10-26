use super::*;
use base64::Engine;
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::header::{AUTHORIZATION, HeaderValue};
use serde_json::json;
use std::io::Read;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const NETWORK_TIMEOUT: Duration = Duration::from_secs(90);
// Complex native-page plans and high-resolution artwork can legitimately
// exceed 90 seconds. Keep connection/discovery bounds and never retry a timeout;
// generation POST calls receive this longer, still bounded response window.
const GENERATION_TIMEOUT: Duration = Duration::from_secs(240);
const IMAGE_JSON_LIMIT: usize = 24 * 1024 * 1024;
const MAX_TOTAL_IMAGE_BYTES: usize = 32 * 1024 * 1024;

/// Clones share reqwest's connection pool. Credentials are attached only to requests, never to
/// this process-wide client; idle connections are capped per host and expire after a minute.
fn shared_client() -> Result<Client, String> {
    static CLIENT: OnceLock<Result<Client, &'static str>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .timeout(NETWORK_TIMEOUT)
                .pool_max_idle_per_host(2)
                .pool_idle_timeout(Duration::from_secs(60))
                .redirect(reqwest::redirect::Policy::none())
                // Even a protocol-level automatic retry is inappropriate for a generation POST.
                .retry(reqwest::retry::never())
                .build()
                .map_err(|_| "Cannot initialize AI HTTP transport")
        })
        .as_ref()
        .cloned()
        .map_err(|message| (*message).into())
}

struct LimitedJson(Vec<u8>);

impl std::io::Write for LimitedJson {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > MAX_JSON_BYTES {
            return Err(std::io::Error::other("AI JSON size limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn validate_value(value: &Value, depth: usize, count: &mut usize) -> Result<(), String> {
    *count = count.saturating_add(1);
    if depth > 64 || *count > 100_000 {
        return Err("AI JSON nesting or item count exceeds limit".into());
    }
    match value {
        Value::Array(items) => {
            for item in items {
                validate_value(item, depth.saturating_add(1), count)?;
            }
        }
        Value::Object(items) => {
            for item in items.values() {
                validate_value(item, depth.saturating_add(1), count)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn encode_json(value: &Value) -> Result<Vec<u8>, String> {
    validate_value(value, 0, &mut 0)?;
    let mut writer = LimitedJson(Vec::new());
    serde_json::to_writer(&mut writer, value).map_err(|_| "AI JSON request exceeds limits or cannot be encoded")?;
    Ok(writer.0)
}

struct Transport<'a> {
    client: Client,
    base: Url,
    provider: ProviderKind,
    key: Option<&'a str>,
}

impl<'a> Transport<'a> {
    fn new(selection: &ModelSelection, key: Option<&'a str>) -> Result<Self, String> {
        let base = validate_endpoint(selection)?;
        if key.is_some_and(|key| !key.is_empty() && base.as_str().contains(key)) {
            return Err("API key must not occur in the endpoint URL".into());
        }
        if matches!(selection.provider, ProviderKind::OpenAi | ProviderKind::Gemini) && key.is_none_or(str::is_empty) {
            return Err("Selected cloud provider requires an API key".into());
        }
        if key.is_some_and(|key| key.len() > 8192 || key.chars().any(char::is_control)) {
            return Err("Invalid API key".into());
        }
        let client = shared_client()?;
        Ok(Self { client, base, provider: selection.provider, key })
    }

    fn url(&self, path: &str) -> Result<Url, String> {
        self.base.join(path).map_err(|_| "Invalid AI request path".into())
    }

    fn request(&self, method: reqwest::Method, url: Url) -> Result<RequestBuilder, String> {
        let generation =
            method == reqwest::Method::POST && matches!(self.provider, ProviderKind::Gemini | ProviderKind::OpenAi | ProviderKind::Ollama);
        let mut request = self.client.request(method, url);
        if generation {
            request = request.timeout(GENERATION_TIMEOUT);
        }
        if let Some(key) = self.key {
            let mut value = if self.provider == ProviderKind::Gemini {
                HeaderValue::from_str(key).map_err(|_| "Invalid API key")?
            } else {
                use zeroize::Zeroize;
                let mut bearer = format!("Bearer {key}");
                let parsed = HeaderValue::from_str(&bearer);
                bearer.zeroize();
                parsed.map_err(|_| "Invalid API key")?
            };
            value.set_sensitive(true);
            request =
                if self.provider == ProviderKind::Gemini { request.header("x-goog-api-key", value) } else { request.header(AUTHORIZATION, value) };
        }
        Ok(request)
    }

    fn json(&self, method: reqwest::Method, url: Url, body: Option<&Value>, cancel: &AtomicBool, limit: usize) -> Result<Value, String> {
        let mut request = self.request(method, url)?;
        if let Some(body) = body {
            let encoded = encode_json(body)?;
            request = request.header("content-type", "application/json").body(encoded);
        }
        let response = send(request, cancel)?;
        let bytes = read_bounded(response, cancel, limit)?;
        serde_json::from_slice(&bytes).map_err(|_| "Provider returned malformed or truncated JSON".into())
    }

    fn get(&self, path: &str, cancel: &AtomicBool, timeout: Duration) -> Result<Value, String> {
        let request = self.request(reqwest::Method::GET, self.url(path)?)?.timeout(timeout);
        let bytes = read_bounded(send(request, cancel)?, cancel, MAX_JSON_BYTES)?;
        serde_json::from_slice(&bytes).map_err(|_| "Provider returned malformed or truncated JSON".into())
    }

    fn post(&self, path: &str, body: &Value, cancel: &AtomicBool, limit: usize) -> Result<Value, String> {
        self.json(reqwest::Method::POST, self.url(path)?, Some(body), cancel, limit)
    }

    fn post_once(&self, path: &str, body: &Value, cancel: &AtomicBool, limit: usize) -> Result<Value, String> {
        let encoded = encode_json(body)?;
        let request = self.request(reqwest::Method::POST, self.url(path)?)?.header("content-type", "application/json").body(encoded);
        let response = send_once(request, cancel)?;
        let bytes = read_bounded(response, cancel, limit)?;
        serde_json::from_slice(&bytes).map_err(|_| "Provider returned malformed or truncated JSON".into())
    }
}

const MAX_LIVE_HTTP_WORKERS: usize = 16;
static LIVE_HTTP_WORKERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static ACTIVE_LOCAL_ORIGINS: OnceLock<std::sync::Mutex<std::collections::BTreeSet<(u8, String)>>> = OnceLock::new();

struct HttpWorkerSlot;

impl HttpWorkerSlot {
    fn acquire() -> Result<Self, String> {
        let mut current = LIVE_HTTP_WORKERS.load(Ordering::Relaxed);
        loop {
            if current >= MAX_LIVE_HTTP_WORKERS {
                return Err("Too many cancelled AI HTTP requests are still shutting down; wait before starting another request".into());
            }
            match LIVE_HTTP_WORKERS.compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => return Ok(Self),
                Err(observed) => current = observed,
            }
        }
    }
}

impl Drop for HttpWorkerSlot {
    fn drop(&mut self) {
        LIVE_HTTP_WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}

struct LocalOriginRelease {
    key: (u8, String),
}

impl Drop for LocalOriginRelease {
    fn drop(&mut self) {
        if let Some(origins) = ACTIVE_LOCAL_ORIGINS.get() {
            let mut guard = origins.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.remove(&self.key);
        }
    }
}

thread_local! {
    static CURRENT_ORIGIN_LEASE: std::cell::RefCell<std::sync::Weak<LocalOriginRelease>> =
        const { std::cell::RefCell::new(std::sync::Weak::new()) };
}

struct LocalOriginLease {
    token: Option<std::sync::Arc<LocalOriginRelease>>,
}

impl LocalOriginLease {
    fn acquire(provider: ProviderKind, base: &Url) -> Result<Self, String> {
        let tag = match provider {
            ProviderKind::Ollama => 1_u8,
            ProviderKind::ComfyUi => 2_u8,
            ProviderKind::OpenAi | ProviderKind::Gemini => return Ok(Self { token: None }),
        };
        let key = (tag, base.origin().ascii_serialization());
        let origins = ACTIVE_LOCAL_ORIGINS.get_or_init(|| std::sync::Mutex::new(std::collections::BTreeSet::new()));
        {
            let mut guard = origins.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if !guard.insert(key.clone()) {
                return Err("Another local inference is running; wait before sending another request".into());
            }
        }
        let token = std::sync::Arc::new(LocalOriginRelease { key });
        let weak = std::sync::Arc::downgrade(&token);
        CURRENT_ORIGIN_LEASE.with(|slot| {
            let _ = slot.replace(weak);
        });
        Ok(Self { token: Some(token) })
    }
}

impl Drop for LocalOriginLease {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            let weak = std::sync::Arc::downgrade(&token);
            CURRENT_ORIGIN_LEASE.with(|slot| {
                let clear = std::sync::Weak::ptr_eq(&slot.borrow(), &weak);
                if clear {
                    let _ = slot.replace(std::sync::Weak::new());
                }
            });
        }
    }
}

fn current_origin_lease() -> Option<std::sync::Arc<LocalOriginRelease>> {
    CURRENT_ORIGIN_LEASE.with(|slot| slot.borrow().upgrade())
}

fn await_cancelable<T: Send + 'static>(
    cancel: &AtomicBool,
    work: impl FnOnce(std::sync::Arc<AtomicBool>) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    check_cancel(cancel)?;
    let worker_slot = HttpWorkerSlot::acquire()?;
    let origin_lease = current_origin_lease();
    let local_lease = if origin_lease.is_some() { super::current_local_lease() } else { None };
    let bg_cancel = std::sync::Arc::new(AtomicBool::new(false));
    let worker_cancel = std::sync::Arc::clone(&bg_cancel);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("dc-ai-http".into())
        .spawn(move || {
            let outcome = {
                let _slot = worker_slot;
                let _local_lease = local_lease;
                let _origin_lease = origin_lease;
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(worker_cancel)))
                    .unwrap_or_else(|_| Err("AI provider HTTP worker failed unexpectedly; no automatic retry was sent".into()))
            };
            let _ = tx.send(outcome);
        })
        .map_err(|_| "Cannot spawn bounded AI HTTP worker".to_string())?;
    loop {
        if cancel.load(Ordering::Relaxed) {
            bg_cancel.store(true, Ordering::Relaxed);
            return Err("AI request cancelled".into());
        }
        match rx.recv_timeout(Duration::from_millis(25)) {
            Ok(result) => {
                if cancel.load(Ordering::Relaxed) {
                    bg_cancel.store(true, Ordering::Relaxed);
                    return Err("AI request cancelled".into());
                }
                return result;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                bg_cancel.store(true, Ordering::Relaxed);
                check_cancel(cancel)?;
                return Err("AI provider HTTP worker disconnected before completion; no automatic retry was sent".into());
            }
        }
    }
}

fn send_raw(request: RequestBuilder, cancel: &AtomicBool) -> Result<Response, String> {
    await_cancelable(cancel, move |_bg_cancel| {
        request.send().map_err(|error| {
            // Do not expose reqwest errors: they can contain full URLs and credentials.
            let detail = if error.is_connect() {
                if error.is_timeout() {
                    "AI provider connection timed out; no automatic retry was sent"
                } else {
                    "AI provider connection failed; no automatic retry was sent"
                }
            } else if error.is_timeout() {
                "AI provider response timed out; request outcome is unknown; no automatic retry was sent"
            } else {
                "AI provider request failed before a complete response; request outcome is unknown; no automatic retry was sent"
            };
            detail.into()
        })
    })
}

fn retry_after_delay_ms(response: &Response) -> Option<u64> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|text| text.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds <= 3)
        .map(|seconds| seconds.saturating_mul(1000))
}

fn sleep_cancelable(mut remaining_ms: u64, cancel: &AtomicBool) -> Result<(), String> {
    check_cancel(cancel)?;
    while remaining_ms > 0 {
        let slice = remaining_ms.min(25);
        std::thread::sleep(Duration::from_millis(slice));
        check_cancel(cancel)?;
        remaining_ms = remaining_ms.saturating_sub(slice);
    }
    Ok(())
}

fn check_http_status(response: Response) -> Result<Response, String> {
    let status = response.status();
    if !status.is_success() {
        let detail = match status.as_u16() {
            400 => "invalid request or unsupported model parameter",
            401 | 403 => "authentication or permission denied",
            404 => "model or endpoint not found",
            422 => "request schema or parameters rejected",
            429 => "rate or quota limit reached",
            502..=504 => "provider temporarily unavailable",
            300..=399 => "redirect refused",
            _ => "provider returned an error",
        };
        // Error bodies may echo keys or confidential prompts. Never include them.
        return Err(format!("AI provider HTTP {}: {detail}", status.as_u16()));
    }
    Ok(response)
}

fn send_once(request: RequestBuilder, cancel: &AtomicBool) -> Result<Response, String> {
    check_cancel(cancel)?;
    let response = send_raw(request, cancel)?;
    check_cancel(cancel)?;
    check_http_status(response)
}

fn send(mut request: RequestBuilder, cancel: &AtomicBool) -> Result<Response, String> {
    let mut attempt = 0u8;
    loop {
        check_cancel(cancel)?;
        let retry_clone = if attempt < 2 { request.try_clone() } else { None };
        // Timeouts, connection errors, and read failures return Err from send_raw and are NEVER retried.
        let response = send_raw(request, cancel)?;
        check_cancel(cancel)?;
        let code = response.status().as_u16();
        if matches!(code, 429 | 503)
            && let Some(next_request) = retry_clone
        {
            let delay_ms = retry_after_delay_ms(&response).unwrap_or(if attempt == 0 { 250 } else { 750 });
            drop(response);
            sleep_cancelable(delay_ms, cancel)?;
            request = next_request;
            attempt = attempt.saturating_add(1);
            continue;
        }
        return check_http_status(response);
    }
}

fn read_bounded(mut response: Response, cancel: &AtomicBool, limit: usize) -> Result<Vec<u8>, String> {
    check_cancel(cancel)?;
    if response.content_length().is_some_and(|size| size > limit as u64) {
        return Err("AI response exceeds size limit".into());
    }
    await_cancelable(cancel, move |bg_cancel| {
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            if bg_cancel.load(Ordering::Relaxed) {
                return Err("AI request cancelled".into());
            }
            let count = response.read(&mut chunk).map_err(|error| response_read_error(&error))?;
            if count == 0 {
                break;
            }
            if bytes.len().saturating_add(count) > limit {
                return Err("AI response exceeds size limit".into());
            }
            let part = chunk.get(..count).ok_or("Invalid AI response chunk")?;
            bytes.extend_from_slice(part);
        }
        Ok(bytes)
    })
}

fn response_read_error(error: &std::io::Error) -> &'static str {
    let is_timeout = |error: &std::io::Error| matches!(error.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock);
    let mut timed_out = is_timeout(error);
    // Blocking reqwest wraps its timeout in an io::Error of kind Other. Inspect types only:
    // formatting either error could expose URLs, authentication or confidential request data.
    let mut cause: Option<&(dyn std::error::Error + 'static)> = error.get_ref().map(|cause| cause as _);
    for _ in 0..16 {
        let Some(current) = cause else { break };
        timed_out |= current.downcast_ref::<reqwest::Error>().is_some_and(reqwest::Error::is_timeout)
            || current.downcast_ref::<std::io::Error>().is_some_and(is_timeout);
        cause = current.source();
    }
    if timed_out {
        "AI provider response body timed out; response may be incomplete; no automatic retry was sent"
    } else {
        "AI provider response body read failed; response may be incomplete; no automatic retry was sent"
    }
}

fn model_id(selection: &ModelSelection) -> Result<&str, String> {
    let model = selection.model.strip_prefix("models/").unwrap_or(&selection.model);
    if model.is_empty() || model.len() > 256 || !model.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_.:/".contains(&c)) {
        return Err("Invalid model identifier".into());
    }
    if model.split('/').any(|part| part == ".." || part == "." || part.is_empty()) {
        return Err("Invalid model identifier".into());
    }
    Ok(model)
}

fn gemini_json_prompt(request: &GenerationRequest) -> Result<String, String> {
    // The native command schema contains open parameter objects and empty image arrays.
    // Gemini can reject that schema before inference. JSON mode carries the same contract
    // as prompt data; the worker still validates every command on its private document.
    const PREFIX: &str = "\n\nOUTPUT JSON CONTRACT:\n";
    let schema = encode_json(&request.json_schema)?;
    if request.prompt.len().saturating_add(request.system.len()).saturating_add(PREFIX.len()).saturating_add(schema.len()) > MAX_JSON_BYTES / 2 {
        return Err("AI prompt and JSON contract exceed size limit".into());
    }
    let schema = std::str::from_utf8(&schema).map_err(|_| "Cannot encode AI JSON contract")?;
    Ok(format!("{}{PREFIX}{schema}", request.prompt))
}

fn schema_declares_type(map: &serde_json::Map<String, Value>, expected: &str) -> bool {
    match map.get("type") {
        Some(Value::String(kind)) => kind == expected,
        Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind.as_str() == Some(expected)),
        _ => false,
    }
}

pub(super) fn gemini_supports_json_schema(schema: &Value) -> bool {
    fn is_valid_type_name(name: &str) -> bool {
        matches!(name, "object" | "array" | "string" | "number" | "integer" | "boolean" | "null")
    }
    fn check_node(node: &Value, depth: usize) -> bool {
        if depth > 32 {
            return false;
        }
        let Some(map) = node.as_object() else {
            return false;
        };
        if map.is_empty()
            || map.contains_key("$ref")
            || map.contains_key("$defs")
            || map.contains_key("definitions")
            || map.contains_key("oneOf")
            || map.contains_key("allOf")
            || map.contains_key("not")
            || map.contains_key("const")
        {
            return false;
        }
        if !map.get("additionalProperties").is_none_or(|ap| ap == &Value::Bool(false) || check_node(ap, depth.saturating_add(1))) {
            return false;
        }
        let has_valid_type = match map.get("type") {
            None => false,
            Some(Value::String(kind)) if is_valid_type_name(kind) => true,
            Some(Value::Array(kinds)) if !kinds.is_empty() && kinds.iter().all(|kind| kind.as_str().is_some_and(is_valid_type_name)) => true,
            Some(_) => return false,
        };
        let has_valid_enum = match map.get("enum") {
            None => false,
            Some(Value::Array(values)) if !values.is_empty() => true,
            Some(_) => return false,
        };
        if schema_declares_type(map, "object") || map.contains_key("properties") {
            let Some(props) = map.get("properties").and_then(Value::as_object) else {
                return false;
            };
            if props.is_empty() || !props.values().all(|prop| check_node(prop, depth.saturating_add(1))) {
                return false;
            }
        }
        if schema_declares_type(map, "array") || map.contains_key("items") || map.contains_key("prefixItems") {
            if map.get("maxItems").and_then(Value::as_u64) == Some(0) {
                return false;
            }
            let valid_items = match map.get("items") {
                None => false,
                Some(items) => check_node(items, depth.saturating_add(1)),
            };
            if map.contains_key("items") && !valid_items {
                return false;
            }
            let valid_prefix_items = match map.get("prefixItems") {
                None => false,
                Some(Value::Array(list)) if !list.is_empty() && list.iter().all(|item| check_node(item, depth.saturating_add(1))) => true,
                Some(_) => return false,
            };
            if !valid_items && !valid_prefix_items {
                return false;
            }
        }
        let has_any_of = match map.get("anyOf") {
            None => false,
            Some(Value::Array(list)) if !list.is_empty() && list.iter().all(|branch| check_node(branch, depth.saturating_add(1))) => true,
            Some(_) => return false,
        };
        has_valid_type || has_any_of || has_valid_enum
    }
    check_node(schema, 0)
}

const GEMINI_MAX_SCHEMA_ANY_OF_BRANCHES: usize = 4;
const GEMINI_MAX_SCHEMA_UNROLLED_WEIGHT: usize = 48;

pub(super) fn gemini_prepare_json_schema(schema: &Value) -> Option<Value> {
    fn prepare_node(node: &Value, depth: usize, weight: &mut usize) -> Option<Value> {
        if depth > 32 || *weight > GEMINI_MAX_SCHEMA_UNROLLED_WEIGHT {
            return None;
        }
        let map = node.as_object()?;
        *weight = weight.saturating_add(1);
        if *weight > GEMINI_MAX_SCHEMA_UNROLLED_WEIGHT {
            return None;
        }
        let mut out = serde_json::Map::with_capacity(map.len());
        for (key, value) in map {
            if matches!(key.as_str(), "minLength" | "maxLength" | "pattern") {
                continue;
            }
            match key.as_str() {
                "properties" => {
                    let props = value.as_object()?;
                    let mut clean_props = serde_json::Map::with_capacity(props.len());
                    for (prop_name, prop_schema) in props {
                        clean_props.insert(prop_name.clone(), prepare_node(prop_schema, depth.saturating_add(1), weight)?);
                    }
                    out.insert(key.clone(), Value::Object(clean_props));
                }
                "items" => {
                    let repeat_raw =
                        map.get("maxItems").and_then(Value::as_u64).or_else(|| map.get("minItems").and_then(Value::as_u64)).unwrap_or(1).clamp(1, 16);
                    let repeat = usize::try_from(repeat_raw).unwrap_or(16);
                    let mut item_weight = 0usize;
                    let clean_items = prepare_node(value, depth.saturating_add(1), &mut item_weight)?;
                    *weight = weight.saturating_add(item_weight.saturating_mul(repeat));
                    if *weight > GEMINI_MAX_SCHEMA_UNROLLED_WEIGHT {
                        return None;
                    }
                    out.insert(key.clone(), clean_items);
                }
                "prefixItems" => {
                    let list = value.as_array()?;
                    let mut clean_list = Vec::with_capacity(list.len());
                    for item in list {
                        clean_list.push(prepare_node(item, depth.saturating_add(1), weight)?);
                    }
                    out.insert(key.clone(), Value::Array(clean_list));
                }
                "anyOf" => {
                    let list = value.as_array()?;
                    if list.len() > GEMINI_MAX_SCHEMA_ANY_OF_BRANCHES {
                        return None;
                    }
                    let mut clean_list = Vec::with_capacity(list.len());
                    for branch in list {
                        clean_list.push(prepare_node(branch, depth.saturating_add(1), weight)?);
                    }
                    out.insert(key.clone(), Value::Array(clean_list));
                }
                _ => {
                    out.insert(key.clone(), value.clone());
                }
            }
        }
        (*weight <= GEMINI_MAX_SCHEMA_UNROLLED_WEIGHT).then_some(Value::Object(out))
    }

    if !gemini_supports_json_schema(schema) {
        return None;
    }
    let mut weight = 0usize;
    prepare_node(schema, 0, &mut weight)
}

fn sanitize_reason_token(raw: Option<&str>) -> Option<String> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() || trimmed.len() > 32 {
        return None;
    }
    trimmed.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_').then(|| trimmed.to_string())
}

fn gemini_thinking_config(model: &str, max_output_tokens: u32) -> Option<Value> {
    let model = model.trim().to_ascii_lowercase();
    let model = model.strip_prefix("models/").unwrap_or(&model);
    if model.contains("-image")
        || model.contains("-tts")
        || model.contains("-live")
        || model.contains("embedding")
        || model.contains("imagen")
        || model.contains("veo")
        || model.contains("lyria")
    {
        return None;
    }
    if model.starts_with("gemini-3") || model.starts_with("gemini-4") {
        return Some(json!({"thinkingLevel": "low"}));
    }
    if model.starts_with("gemini-2.5-pro") || model == "gemini-pro" || model == "gemini-pro-latest" {
        let budget = if max_output_tokens < 256 { 128 } else { (max_output_tokens / 8).clamp(128, 2048) };
        return Some(json!({"thinkingBudget": budget}));
    }
    if model.contains("flash-lite") {
        return Some(json!({"thinkingBudget": 0}));
    }
    if model.starts_with("gemini-2.5-flash") || model == "gemini-flash" || model == "gemini-flash-latest" {
        let budget = if max_output_tokens < 1024 { 0 } else { (max_output_tokens / 8).clamp(256, 1024) };
        return Some(json!({"thinkingBudget": budget}));
    }
    None
}

pub(super) fn generation_body(selection: &ModelSelection, request: &GenerationRequest) -> Result<Value, String> {
    if request.prompt.len().saturating_add(request.system.len()) > MAX_JSON_BYTES / 2 || !request.json_schema.is_object() {
        return Err("AI prompt is too large or schema is not an object".into());
    }
    if request.max_output_tokens == 0 || request.max_output_tokens > 131_072 {
        return Err("AI output token limit must be between 1 and 131072".into());
    }
    let model = model_id(selection)?;
    encode_json(&request.json_schema)?;
    let mut total_image_bytes = 0usize;
    if request.images.len() > 8 {
        return Err("AI reference image count exceeds limit".into());
    }
    let mut encoded_images = Vec::new();
    for input in &request.images {
        if !matches!(input.mime_type.as_str(), "image/png" | "image/jpeg" | "image/webp") {
            return Err("Unsupported AI reference image format".into());
        }
        total_image_bytes = total_image_bytes.saturating_add(input.bytes.len());
        if input.bytes.is_empty() || total_image_bytes > 2 * 1024 * 1024 {
            return Err("AI reference images exceed the 2 MiB combined limit or are empty".into());
        }
        encoded_images.push(base64::engine::general_purpose::STANDARD.encode(&input.bytes));
    }
    let mut body = match selection.provider {
        ProviderKind::OpenAi => json!({"model":model,"instructions":request.system,"input":request.prompt,"store":false,
            "max_output_tokens":request.max_output_tokens,"text":{"format":{"type":"json_schema","name":"designcraft_plan","schema":request.json_schema,"strict":false}}}),
        ProviderKind::Gemini => {
            let mut generation_config = json!({"responseMimeType":"application/json","maxOutputTokens":request.max_output_tokens});
            let normalized_model = model.to_ascii_lowercase();
            if (normalized_model.starts_with("gemini-2.5") || normalized_model.starts_with("gemini-3") || normalized_model.starts_with("gemini-4"))
                && let Some(prepared_schema) = gemini_prepare_json_schema(&request.json_schema)
                && let Some(config) = generation_config.as_object_mut()
            {
                config.insert("responseJsonSchema".into(), prepared_schema);
            }
            if let Some(thinking_config) = gemini_thinking_config(model, request.max_output_tokens)
                && let Some(config) = generation_config.as_object_mut()
            {
                config.insert("thinkingConfig".into(), thinking_config);
            }
            json!({"systemInstruction":{"parts":[{"text":request.system}]},"contents":[{"role":"user","parts":[{"text":gemini_json_prompt(request)?}]}],
            "generationConfig":generation_config})
        }
        ProviderKind::Ollama => {
            request.resources.validate()?;
            if request.max_output_tokens >= request.resources.context_tokens {
                return Err("Local output limit must be smaller than the context window".into());
            }
            let available = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
            let threads = usize::from(request.resources.cpu_threads).min(available.saturating_sub(1).max(1));
            let mut options = json!({"num_predict":request.max_output_tokens,"num_ctx":request.resources.context_tokens,
                "num_thread":threads,"num_batch":request.resources.batch_tokens});
            if !request.resources.use_gpu {
                options["num_gpu"] = json!(0);
            }
            json!({"model":model,"stream":false,"messages":[{"role":"system","content":request.system},{"role":"user","content":request.prompt}],
                "format":request.json_schema,"options":options,"keep_alive":request.resources.keep_alive_seconds})
        }
        ProviderKind::ComfyUi => return Err("ComfyUI supports image workflows, not layout text generation".into()),
    };
    if !request.images.is_empty() {
        match selection.provider {
            ProviderKind::OpenAi => {
                let mut content = vec![json!({"type":"input_text","text":request.prompt})];
                for (input, encoded) in request.images.iter().zip(&encoded_images) {
                    content.push(json!({"type":"input_image","image_url":format!("data:{};base64,{encoded}",input.mime_type)}));
                }
                body["input"] = json!([{"role":"user","content":content}]);
            }
            ProviderKind::Gemini => {
                let mut parts = vec![json!({"text":gemini_json_prompt(request)?})];
                for (input, encoded) in request.images.iter().zip(&encoded_images) {
                    parts.push(json!({"inlineData":{"mimeType":input.mime_type,"data":encoded}}));
                }
                body["contents"] = json!([{"role":"user","parts":parts}]);
            }
            ProviderKind::Ollama => {
                if let Some(message) = body.pointer_mut("/messages/1") {
                    message["images"] = json!(encoded_images);
                }
            }
            ProviderKind::ComfyUi => return Err("ComfyUI does not support reference analysis".into()),
        }
    }
    Ok(body)
}

pub(super) fn generate(
    selection: &ModelSelection,
    key: Option<&str>,
    request: &GenerationRequest,
    cancel: &AtomicBool,
) -> Result<GenerationResponse, String> {
    let transport = Transport::new(selection, key)?;
    let _origin_lease = LocalOriginLease::acquire(transport.provider, &transport.base)?;
    generate_with_post(selection, request, cancel, |path, body, once| {
        if once { transport.post_once(path, body, cancel, MAX_JSON_BYTES) } else { transport.post(path, body, cancel, MAX_JSON_BYTES) }
    })
}

pub(super) fn generate_with_post(
    selection: &ModelSelection,
    request: &GenerationRequest,
    cancel: &AtomicBool,
    mut post: impl FnMut(&str, &Value, bool) -> Result<Value, String>,
) -> Result<GenerationResponse, String> {
    let body = generation_body(selection, request)?;
    let path = match selection.provider {
        ProviderKind::OpenAi => "responses".into(),
        ProviderKind::Gemini => format!("models/{}:generateContent", model_id(selection)?),
        ProviderKind::Ollama => "api/chat".into(),
        ProviderKind::ComfyUi => return Err("ComfyUI requires an image workflow".into()),
    };
    let response = match post(&path, &body, false) {
        Ok(value) => value,
        Err(error)
            if selection.provider == ProviderKind::Gemini
                && error.starts_with("AI provider HTTP 400:")
                && body
                    .get("generationConfig")
                    .and_then(Value::as_object)
                    .is_some_and(|cfg| cfg.contains_key("responseJsonSchema") || cfg.contains_key("thinkingConfig")) =>
        {
            crate::check_cancel(cancel)?;
            let mut fallback_body = body;
            if let Some(cfg) = fallback_body.get_mut("generationConfig").and_then(Value::as_object_mut) {
                cfg.remove("responseJsonSchema");
                cfg.remove("thinkingConfig");
            }
            post(&path, &fallback_body, true)?
        }
        Err(error) => return Err(error),
    };
    parse_generation(selection.provider, &response)
}

pub(super) fn parse_usage(provider: ProviderKind, value: &Value) -> Usage {
    let usage = if provider == ProviderKind::Gemini { value.get("usageMetadata") } else { value.get("usage") };
    let number = |name: &str| usage.and_then(|u| u.get(name)).and_then(Value::as_u64);
    match provider {
        ProviderKind::OpenAi => Usage {
            input_tokens: number("input_tokens"),
            output_tokens: number("output_tokens"),
            cached_input_tokens: usage.and_then(|u| u.pointer("/input_tokens_details/cached_tokens")).and_then(Value::as_u64),
            reasoning_tokens: usage.and_then(|u| u.pointer("/output_tokens_details/reasoning_tokens")).and_then(Value::as_u64),
            cache_write_tokens: None,
        },
        ProviderKind::Gemini => Usage {
            input_tokens: number("promptTokenCount"),
            output_tokens: number("candidatesTokenCount").and_then(|candidate| match number("thoughtsTokenCount") {
                Some(thoughts) => candidate.checked_add(thoughts),
                None => Some(candidate),
            }),
            cached_input_tokens: number("cachedContentTokenCount"),
            reasoning_tokens: number("thoughtsTokenCount"),
            cache_write_tokens: None,
        },
        ProviderKind::Ollama => Usage {
            input_tokens: value.get("prompt_eval_count").and_then(Value::as_u64),
            output_tokens: value.get("eval_count").and_then(Value::as_u64),
            cached_input_tokens: value.get("prompt_eval_cached_count").and_then(Value::as_u64),
            reasoning_tokens: None,
            cache_write_tokens: None,
        },
        ProviderKind::ComfyUi => Usage::default(),
    }
}

fn gemini_candidate_text(candidate: &Value) -> String {
    candidate
        .pointer("/content/parts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|p| p.get("thought").and_then(Value::as_bool) != Some(true))
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .collect::<String>()
}

pub(super) fn parse_generation(provider: ProviderKind, value: &Value) -> Result<GenerationResponse, String> {
    let usage = parse_usage(provider, value);
    let (text, error) = match generation_text(provider, value) {
        Ok(text) => (text, None),
        Err(error) => {
            let partial = if provider == ProviderKind::Gemini
                && let Some(candidate) = value.get("candidates").and_then(Value::as_array).and_then(|v| v.first())
                && candidate.get("finishReason").and_then(Value::as_str) == Some("MAX_TOKENS")
            {
                let text = gemini_candidate_text(candidate);
                if text.len() <= MAX_JSON_BYTES { text } else { String::new() }
            } else {
                String::new()
            };
            (partial, Some(error))
        }
    };
    Ok(GenerationResponse { text, usage, error })
}

fn generation_text(provider: ProviderKind, value: &Value) -> Result<String, String> {
    if value.get("error").is_some_and(|v| !v.is_null()) {
        return Err("AI provider returned an error".into());
    }
    let text = match provider {
        ProviderKind::OpenAi => {
            if value.get("status").and_then(Value::as_str) != Some("completed") {
                return Err("AI response did not complete (refused, interrupted, or output limit reached)".into());
            }
            let mut text = String::new();
            for item in value.get("output").and_then(Value::as_array).into_iter().flatten() {
                for part in item.get("content").and_then(Value::as_array).into_iter().flatten() {
                    if part.get("type").and_then(Value::as_str) == Some("refusal") {
                        return Err("AI provider refused the request".into());
                    }
                    if part.get("type").and_then(Value::as_str) == Some("output_text")
                        && let Some(part) = part.get("text").and_then(Value::as_str)
                    {
                        text.push_str(part);
                    }
                }
            }
            text
        }
        ProviderKind::Gemini => {
            let Some(candidate) = value.get("candidates").and_then(Value::as_array).and_then(|v| v.first()) else {
                let block_reason = sanitize_reason_token(value.pointer("/promptFeedback/blockReason").and_then(Value::as_str));
                return Err(match block_reason {
                    Some(reason) => format!("Gemini returned no candidate ({reason})"),
                    None => "Gemini returned no candidate (possibly blocked)".into(),
                });
            };
            let finish_reason = candidate.get("finishReason").and_then(Value::as_str);
            let text = gemini_candidate_text(candidate);
            if finish_reason != Some("STOP") {
                // If MAX_TOKENS fired on the exact closing boundary of a complete JSON object,
                // let the worker validate the JSON; otherwise report the abnormal finish.
                let boundary_complete = finish_reason == Some("MAX_TOKENS")
                    && !text.is_empty()
                    && text.len() <= MAX_JSON_BYTES
                    && serde_json::from_str::<Value>(&text).is_ok_and(|v| v.is_object());
                if !boundary_complete {
                    return Err(match sanitize_reason_token(finish_reason) {
                        Some(reason) => format!("Gemini response did not finish normally ({reason})"),
                        None => "Gemini response did not finish normally".into(),
                    });
                }
            }
            text
        }
        ProviderKind::Ollama => {
            if value.get("done").and_then(Value::as_bool) != Some(true) || value.get("done_reason").and_then(Value::as_str) == Some("length") {
                return Err("Ollama response incomplete or output limit reached".into());
            }
            value.pointer("/message/content").and_then(Value::as_str).ok_or("Ollama response has no message content")?.to_string()
        }
        ProviderKind::ComfyUi => return Err("ComfyUI does not provide structured text generation".into()),
    };
    if text.is_empty() || text.len() > MAX_JSON_BYTES {
        return Err("AI provider returned empty or excessive text".into());
    }
    // Model JSON validation belongs to the worker so repair attempts preserve
    // the measured usage of this response, including malformed model output.
    Ok(text)
}

pub(super) fn list_models(selection: &ModelSelection, key: Option<&str>, cancel: &AtomicBool) -> Result<Vec<ModelInfo>, String> {
    let transport = Transport::new(selection, key)?;
    let mut models = Vec::new();
    let path = match selection.provider {
        ProviderKind::OpenAi => "models",
        ProviderKind::Gemini => "models",
        ProviderKind::Ollama => "api/tags",
        ProviderKind::ComfyUi => "models/checkpoints",
    };
    let mut url = transport.url(path)?;
    // Gemini discovery is paginated; bounded pages prevent hostile servers looping forever.
    for _ in 0..20 {
        let value = transport.json(reqwest::Method::GET, url.clone(), None, cancel, MAX_JSON_BYTES)?;
        if selection.provider == ProviderKind::ComfyUi {
            let list = value.as_array().ok_or("ComfyUI returned an invalid checkpoint list")?;
            for item in list {
                if let Some(id) = item.as_str() {
                    models.push(ModelInfo { id: id.into(), supports_generate_content: Some(false) });
                }
            }
        } else {
            let list = value
                .get(if selection.provider == ProviderKind::OpenAi { "data" } else { "models" })
                .and_then(Value::as_array)
                .ok_or("AI provider returned an invalid model list")?;
            for item in list {
                let field = if selection.provider == ProviderKind::OpenAi { "id" } else { "name" };
                if let Some(id) = item.get(field).and_then(Value::as_str) {
                    let supported = if selection.provider == ProviderKind::Gemini {
                        item.get("supportedGenerationMethods")
                            .and_then(Value::as_array)
                            .map(|list| list.iter().any(|v| v.as_str() == Some("generateContent")))
                    } else {
                        None
                    };
                    models.push(ModelInfo { id: id.strip_prefix("models/").unwrap_or(id).into(), supports_generate_content: supported });
                }
            }
        }
        if models.len() > 10_000 {
            return Err("AI model discovery exceeds limit".into());
        }
        let next = value.get("nextPageToken").and_then(Value::as_str).filter(|s| !s.is_empty());
        if selection.provider != ProviderKind::Gemini || next.is_none() {
            return Ok(models);
        }
        url = transport.url(path)?;
        if let Some(next) = next {
            if next.len() > 4096 {
                return Err("Invalid AI model pagination token".into());
            }
            url.query_pairs_mut().append_pair("pageToken", next);
        }
    }
    Err("AI model discovery exceeded pagination limit".into())
}

fn image(bytes: Vec<u8>, filename: String) -> Result<GeneratedImage, String> {
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
        return Err("Generated image exceeds size limit or is empty".into());
    }
    let mime_type = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else {
        return Err("Provider returned unsupported image data".into());
    };
    let filename = if filename.starts_with("generated-") {
        let extension = match mime_type {
            "image/jpeg" => "jpg",
            "image/webp" => "webp",
            _ => "png",
        };
        format!("{}.{}", filename.rsplit_once('.').map_or(filename.as_str(), |(base, _)| base), extension)
    } else {
        filename
    };
    Ok(GeneratedImage { bytes, filename, mime_type: mime_type.into() })
}

fn decode_image(encoded: &str, filename: String) -> Result<GeneratedImage, String> {
    if encoded.len() > MAX_IMAGE_BYTES.saturating_mul(4) / 3 + 8 {
        return Err("Encoded image exceeds size limit".into());
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(encoded).map_err(|_| "Provider returned invalid base64 image")?;
    image(bytes, filename)
}

pub(super) fn generate_images(
    selection: &ModelSelection,
    key: Option<&str>,
    request: &ImageRequest,
    cancel: &AtomicBool,
) -> Result<ImageResponse, String> {
    if request.prompt.is_empty() || request.prompt.len() > MAX_JSON_BYTES / 2 {
        return Err("Image prompt is empty or exceeds size limit".into());
    }
    if selection.provider == ProviderKind::ComfyUi {
        let comfy = request.comfy.as_ref().ok_or("ComfyUI needs a user supplied API workflow")?;
        let workflow = bind_comfy_workflow(&comfy.workflow, &request.prompt, &selection.model)?;
        let bound = ComfyRequest { workflow, poll_interval_ms: comfy.poll_interval_ms, timeout_seconds: comfy.timeout_seconds };
        return Ok(ImageResponse { images: generate_comfy(selection, key, &bound, cancel)?, usage: Usage::default(), error: None });
    }
    if selection.provider == ProviderKind::Ollama {
        return Err("Ollama image generation is unsupported; choose an explicit image provider".into());
    }
    if request.size.as_ref().is_some_and(|value| value.len() > 64) || request.quality.as_ref().is_some_and(|value| value.len() > 64) {
        return Err("Image size or quality option exceeds length limit".into());
    }
    let model = model_id(selection)?;
    let body = image_generation_body(selection.provider, model, request)?;
    let path = if selection.provider == ProviderKind::OpenAi { "images/generations".into() } else { format!("models/{model}:generateContent") };
    let transport = Transport::new(selection, key)?;
    let value = transport.post(&path, &body, cancel, IMAGE_JSON_LIMIT)?;
    parse_images(selection.provider, &value, cancel)
}

pub(super) fn image_generation_body(provider: ProviderKind, model: &str, request: &ImageRequest) -> Result<Value, String> {
    if provider == ProviderKind::OpenAi {
        // GPT Image models return base64 by default; DALL-E defaults to ephemeral URLs unless b64_json is requested.
        let mut body = json!({"model":model,"prompt":request.prompt,"n":1});
        if model.trim().to_ascii_lowercase().starts_with("dall-e") {
            body["response_format"] = json!("b64_json");
        }
        if let Some(size) = &request.size {
            body["size"] = json!(size);
        }
        if let Some(quality) = &request.quality {
            body["quality"] = json!(quality);
        }
        Ok(body)
    } else {
        let mut config = json!({"responseModalities":["IMAGE"]});
        // Gemini's native image config uses an aspect ratio, not an OpenAI pixel size.
        // https://ai.google.dev/gemini-api/docs/image-generation
        if let Some(size) = &request.size {
            let (ratio, tier) = size.split_once('@').map_or((size.as_str(), None), |(ratio, tier)| (ratio, Some(tier)));
            if super::gemini_base_pixels(ratio).is_none() {
                return Err("Unsupported Gemini image aspect ratio".into());
            }
            let lower_model = model.trim().to_ascii_lowercase();
            let normalized_model = lower_model.strip_prefix("models/").unwrap_or(&lower_model);
            if tier.is_none() && normalized_model.starts_with("gemini-") && !super::gemini_print_ratio_supported(normalized_model, ratio) {
                return Err("Unsupported Gemini model for requested image aspect ratio".into());
            }
            config["imageConfig"] = json!({"aspectRatio":ratio});
            if let Some(tier) = tier {
                if !super::gemini_has_print_tiers(normalized_model)
                    || !super::gemini_print_ratio_supported(normalized_model, ratio)
                    || !matches!(tier, "1K" | "2K" | "4K")
                {
                    return Err("Unsupported Gemini model or native image resolution tier (1K, 2K, 4K)".into());
                }
                config["imageConfig"]["imageSize"] = json!(tier);
            }
        }
        Ok(json!({"contents":[{"parts":[{"text":request.prompt}]}],"generationConfig":config}))
    }
}

/// Bind only explicit user-supplied placeholders; never infer graph node IDs.
pub(super) fn bind_comfy_workflow(workflow: &Value, prompt: &str, model: &str) -> Result<Value, String> {
    encode_json(workflow)?;
    if model.len() > 1024 || model.chars().any(char::is_control) {
        return Err("Invalid ComfyUI model name".into());
    }
    let mut bindings = WorkflowBindings { prompt, model, prompt_count: 0, model_count: 0, output_bytes: 0 };
    let bound = bindings.value(workflow)?;
    if bindings.prompt_count == 0 {
        return Err("ComfyUI workflow must bind {{prompt}} in a string value".into());
    }
    if !model.is_empty() && bindings.model_count == 0 {
        return Err("ComfyUI workflow must bind the selected model using {{model}}".into());
    }
    encode_json(&bound)?;
    Ok(bound)
}

struct WorkflowBindings<'a> {
    prompt: &'a str,
    model: &'a str,
    prompt_count: usize,
    model_count: usize,
    output_bytes: usize,
}

impl WorkflowBindings<'_> {
    fn append(&mut self, output: &mut String, part: &str) -> Result<(), String> {
        self.output_bytes = self.output_bytes.saturating_add(part.len());
        if self.output_bytes > MAX_JSON_BYTES {
            return Err("ComfyUI workflow placeholder expansion exceeds size limit".into());
        }
        output.push_str(part);
        Ok(())
    }

    fn string(&mut self, mut template: &str) -> Result<String, String> {
        let mut output = String::new();
        loop {
            let next = match (template.find("{{prompt}}"), template.find("{{model}}")) {
                (Some(prompt), Some(model)) => Some((prompt.min(model), prompt <= model)),
                (Some(prompt), None) => Some((prompt, true)),
                (None, Some(model)) => Some((model, false)),
                (None, None) => None,
            };
            let Some((offset, is_prompt)) = next else {
                self.append(&mut output, template)?;
                return Ok(output);
            };
            self.append(&mut output, template.get(..offset).ok_or("Invalid workflow placeholder boundary")?)?;
            let (replacement, marker_len) = if is_prompt {
                self.prompt_count = self.prompt_count.saturating_add(1);
                (self.prompt, 10usize)
            } else {
                self.model_count = self.model_count.saturating_add(1);
                (self.model, 9usize)
            };
            self.append(&mut output, replacement)?;
            template = template.get(offset.saturating_add(marker_len)..).ok_or("Invalid workflow placeholder boundary")?;
        }
    }

    fn value(&mut self, value: &Value) -> Result<Value, String> {
        // Source depth/count were checked before this bounded traversal.
        match value {
            Value::String(text) => Ok(Value::String(self.string(text)?)),
            Value::Array(items) => items.iter().map(|item| self.value(item)).collect::<Result<Vec<_>, _>>().map(Value::Array),
            Value::Object(items) => items
                .iter()
                .map(|(key, value)| Ok((key.clone(), self.value(value)?)))
                .collect::<Result<serde_json::Map<_, _>, String>>()
                .map(Value::Object),
            _ => Ok(value.clone()),
        }
    }
}

pub(super) fn parse_images(provider: ProviderKind, value: &Value, cancel: &AtomicBool) -> Result<ImageResponse, String> {
    let usage = parse_usage(provider, value);
    let (images, error) = match image_contents(provider, value, cancel) {
        Ok(images) => (images, None),
        Err(error) => (Vec::new(), Some(error)),
    };
    Ok(ImageResponse { images, usage, error })
}

fn image_contents(provider: ProviderKind, value: &Value, cancel: &AtomicBool) -> Result<Vec<GeneratedImage>, String> {
    let mut images = Vec::new();
    let mut total: usize = 0;
    let mut add = |encoded: &str| -> Result<(), String> {
        check_cancel(cancel)?;
        if images.len() >= 8 {
            return Err("AI image count exceeds limit".into());
        }
        let image = decode_image(encoded, format!("generated-{}.png", images.len().saturating_add(1)))?;
        total = total.saturating_add(image.bytes.len());
        if total > MAX_TOTAL_IMAGE_BYTES {
            return Err("AI images exceed total size limit".into());
        }
        images.push(image);
        Ok(())
    };
    if provider == ProviderKind::OpenAi {
        let data = value.get("data").and_then(Value::as_array).ok_or("OpenAI image response has no image data")?;
        for item in data {
            add(item.get("b64_json").and_then(Value::as_str).ok_or("OpenAI did not return inline image data; URL responses are unsupported")?)?;
        }
    } else {
        let candidates = value.get("candidates").and_then(Value::as_array);
        if candidates.is_none_or(Vec::is_empty)
            && let Some(reason) = sanitize_reason_token(value.pointer("/promptFeedback/blockReason").and_then(Value::as_str))
        {
            return Err(format!("Gemini image response blocked ({reason})"));
        }
        for candidate in candidates.into_iter().flatten() {
            let finish_reason = candidate.get("finishReason").and_then(Value::as_str);
            if finish_reason != Some("STOP") {
                return Err(match sanitize_reason_token(finish_reason) {
                    Some(reason) => format!("Gemini image response did not finish normally ({reason})"),
                    None => "Gemini image response did not finish normally".into(),
                });
            }
            for part in candidate.pointer("/content/parts").and_then(Value::as_array).into_iter().flatten() {
                // Gemini image models may return draft thought images before the
                // final artwork. They are billed usage, not placement outputs.
                if part.get("thought").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                if let Some(encoded) = part.pointer("/inlineData/data").and_then(Value::as_str) {
                    add(encoded)?;
                }
            }
        }
    }
    if images.is_empty() {
        return Err("Selected model returned no generated images".into());
    }
    Ok(images)
}

pub(super) fn generate_comfy(
    selection: &ModelSelection,
    key: Option<&str>,
    request: &ComfyRequest,
    cancel: &AtomicBool,
) -> Result<Vec<GeneratedImage>, String> {
    if selection.provider != ProviderKind::ComfyUi {
        return Err("Image workflow requires the ComfyUI provider".into());
    }
    if !request.workflow.is_object() || request.workflow.as_object().is_none_or(|o| o.is_empty()) {
        return Err("ComfyUI needs a nonempty API workflow object".into());
    }
    encode_json(&request.workflow)?;
    if !(1..=600).contains(&request.timeout_seconds) || !(50..=5000).contains(&request.poll_interval_ms) {
        return Err("ComfyUI timeout or poll interval outside supported limits".into());
    }
    let transport = Transport::new(selection, key)?;
    let _origin_lease = LocalOriginLease::acquire(transport.provider, &transport.base)?;
    check_cancel(cancel)?;
    // Once submitted, capture its id even if cancellation arrives while waiting
    // for the response, so cleanup can address exactly this pending queue item.
    let submission = AtomicBool::new(false);
    let queued = transport.post_once("prompt", &json!({"prompt":request.workflow}), &submission, MAX_JSON_BYTES)?;
    let id = queued.get("prompt_id").and_then(Value::as_str).ok_or("ComfyUI rejected the workflow or omitted prompt_id")?;
    if id.is_empty() || id.len() > 128 || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
        return Err("ComfyUI returned invalid prompt_id".into());
    }
    let deadline = Instant::now().checked_add(Duration::from_secs(request.timeout_seconds)).ok_or("Invalid ComfyUI deadline")?;
    let result = poll_comfy(&transport, id, request, deadline, cancel);
    if result.is_err() {
        // A DELETE list only removes our pending job. Never interrupt a running
        // workflow globally; it could belong to somebody else by this point.
        let cleanup = AtomicBool::new(false);
        if let Ok(url) = transport.url("queue")
            && let Ok(request) = transport.request(reqwest::Method::POST, url)
        {
            let _ = send_once(request.timeout(Duration::from_secs(3)).json(&json!({"delete":[id]})), &cleanup);
        }
    }
    result
}

fn poll_comfy(
    transport: &Transport<'_>,
    id: &str,
    request: &ComfyRequest,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<Vec<GeneratedImage>, String> {
    loop {
        check_cancel(cancel)?;
        if Instant::now() >= deadline {
            return Err("ComfyUI job timed out; only its pending queue entry was removed".into());
        }
        let remaining = deadline.saturating_duration_since(Instant::now()).min(NETWORK_TIMEOUT);
        let value = transport.get(&format!("history/{id}"), cancel, remaining)?;
        if let Some(job) = value.get(id) {
            if job.pointer("/status/status_str").and_then(Value::as_str) == Some("error") {
                return Err("ComfyUI workflow failed".into());
            }
            if let Some(outputs) = job.get("outputs").and_then(Value::as_object) {
                let mut images = Vec::new();
                let mut total: usize = 0;
                for output in outputs.values() {
                    for file in output.get("images").and_then(Value::as_array).into_iter().flatten() {
                        check_cancel(cancel)?;
                        if images.len() >= 8 {
                            return Err("ComfyUI image count exceeds limit".into());
                        }
                        let filename = file.get("filename").and_then(Value::as_str).ok_or("ComfyUI image missing filename")?;
                        let subfolder = file.get("subfolder").and_then(Value::as_str).unwrap_or("");
                        let kind = file.get("type").and_then(Value::as_str).unwrap_or("output");
                        if filename.len() > 512 || subfolder.len() > 1024 || !matches!(kind, "output" | "temp") {
                            return Err("Invalid ComfyUI image metadata".into());
                        }
                        let mut url = transport.url("view")?;
                        url.query_pairs_mut().append_pair("filename", filename).append_pair("subfolder", subfolder).append_pair("type", kind);
                        let response = send(transport.request(reqwest::Method::GET, url)?, cancel)?;
                        let bytes = read_bounded(response, cancel, MAX_IMAGE_BYTES)?;
                        total = total.saturating_add(bytes.len());
                        if total > MAX_TOTAL_IMAGE_BYTES {
                            return Err("ComfyUI images exceed total size limit".into());
                        }
                        // Only a basename is returned to callers; this metadata
                        // must never be treated as a trusted filesystem path.
                        let basename = filename.rsplit(['/', '\\']).next().unwrap_or("generated.png").to_string();
                        images.push(image(bytes, basename)?);
                    }
                }
                if images.is_empty() {
                    return Err("ComfyUI completed without supported image outputs".into());
                }
                return Ok(images);
            }
        }
        // Short slices make cancellation responsive between bounded HTTP calls.
        sleep_cancelable(request.poll_interval_ms, cancel)?;
    }
}

#[cfg(test)]
mod transport_http_tests {
    use super::*;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};

    fn selection(endpoint: &str) -> ModelSelection {
        ModelSelection { provider: ProviderKind::Ollama, model: "synthetic-transport".into(), endpoint: Some(endpoint.into()) }
    }

    fn request_text(socket: &mut TcpStream) -> std::io::Result<String> {
        socket.set_read_timeout(Some(Duration::from_secs(2)))?;
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 1024];
        while bytes.len() < 65536 {
            let n = socket.read(&mut buffer)?;
            if n == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            bytes.extend_from_slice(&buffer[..n]);
            let text = String::from_utf8_lossy(&bytes);
            if let Some((headers, body)) = text.split_once("\r\n\r\n") {
                let length = headers
                    .lines()
                    .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|n| n.trim().parse::<usize>().ok()))
                    .unwrap_or(0);
                if body.len() >= length {
                    return Ok(text.into_owned());
                }
            }
        }
        Err(std::io::Error::other("synthetic request exceeds its fixture limit"))
    }

    fn assert_redacted(error: &str) {
        for private in ["synthetic-private-key", "synthetic-private-query", "synthetic-private-prompt", "127.0.0.1", "http:", "https:"] {
            assert!(!error.contains(private), "transport diagnostic contains private request data");
        }
    }

    #[test]
    fn transport_http_reuses_one_connection_without_retaining_request_credentials() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut captured = Vec::new();
            for _ in 0..3 {
                let Ok(request) = request_text(&mut socket) else { break };
                captured.push(request);
                if socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\n{}").is_err() {
                    break;
                }
                socket.flush().unwrap();
            }
            captured
        });
        let selection = selection(&endpoint);
        let mut results = Vec::new();
        for key in [Some("synthetic-private-key"), Some("different-synthetic-key"), None] {
            let transport = Transport::new(&selection, key).unwrap();
            let request = transport.request(reqwest::Method::POST, transport.url("synthetic").unwrap()).unwrap().timeout(Duration::from_secs(1));
            let result = send(request.json(&json!({"prompt":"synthetic-private-prompt"})), &AtomicBool::new(false))
                .and_then(|response| read_bounded(response, &AtomicBool::new(false), 1024));
            results.push(result);
        }
        let captured = server.join().unwrap();
        assert!(results.iter().all(Result::is_ok), "independent transports must reuse the same live connection");
        assert_eq!(captured.len(), 3, "all three requests use one accepted socket");
        assert!(captured[0].contains("Bearer synthetic-private-key"));
        assert!(captured[1].contains("Bearer different-synthetic-key") && !captured[1].contains("synthetic-private-key"));
        assert!(!captured[2].to_ascii_lowercase().contains("authorization:"));
        let transport = Transport::new(&selection, Some("synthetic-private-key")).unwrap();
        let request = transport.request(reqwest::Method::GET, transport.url("synthetic").unwrap()).unwrap().build().unwrap();
        assert!(request.headers().get(AUTHORIZATION).unwrap().is_sensitive());
    }

    #[test]
    fn transport_http_response_timeout_is_redacted_and_does_not_repeat_post() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let first = request_text(&mut socket).unwrap();
            std::thread::sleep(Duration::from_millis(300));
            drop(socket);
            listener.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_millis(150);
            let mut posts = usize::from(first.starts_with("POST "));
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        if request_text(&mut socket).is_ok_and(|request| request.starts_with("POST ")) {
                            posts += 1;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(5)),
                    Err(error) => panic!("synthetic server accept failed: {error}"),
                }
            }
            posts
        });
        let transport = Transport::new(&selection(&endpoint), Some("synthetic-private-key")).unwrap();
        let request = transport
            .request(reqwest::Method::POST, transport.url("slow?trace=synthetic-private-query").unwrap())
            .unwrap()
            .timeout(Duration::from_millis(100))
            .json(&json!({"prompt":"synthetic-private-prompt"}));
        let error = send(request, &AtomicBool::new(false)).unwrap_err();
        assert_eq!(server.join().unwrap(), 1, "an ambiguous billable POST must never be repeated automatically");
        assert!(error.contains("response timed out") && error.contains("unknown"), "{error}");
        assert_redacted(&error);
    }

    #[test]
    fn transport_http_connect_timeout_is_distinct_and_redacted() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("https://{}/synthetic-private-query", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut hello = [0u8; 4096];
            let _ = socket.read(&mut hello);
            // TCP connects, but this synthetic peer never completes the TLS handshake.
            std::thread::sleep(Duration::from_millis(300));
        });
        let client = Client::builder().connect_timeout(Duration::from_millis(100)).timeout(Duration::from_secs(1)).build().unwrap();
        let error = send(client.post(endpoint), &AtomicBool::new(false)).unwrap_err();
        server.join().unwrap();
        assert!(error.contains("connection timed out"), "{error}");
        assert_redacted(&error);
    }

    #[test]
    fn transport_http_body_timeout_and_truncation_have_distinct_redacted_diagnostics() {
        for stalled in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("http://{}/", listener.local_addr().unwrap());
            let server = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                request_text(&mut socket).unwrap();
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\nConnection: close\r\n\r\n{").unwrap();
                socket.flush().unwrap();
                if stalled {
                    std::thread::sleep(Duration::from_millis(300));
                }
            });
            let transport = Transport::new(&selection(&endpoint), Some("synthetic-private-key")).unwrap();
            let request = transport
                .request(reqwest::Method::POST, transport.url("body?trace=synthetic-private-query").unwrap())
                .unwrap()
                .timeout(Duration::from_millis(100))
                .json(&json!({"prompt":"synthetic-private-prompt"}));
            let response = send(request, &AtomicBool::new(false)).unwrap();
            let error = read_bounded(response, &AtomicBool::new(false), 1024).unwrap_err();
            server.join().unwrap();
            assert!(error.contains(if stalled { "response body timed out" } else { "response body read failed" }), "{error}");
            assert_redacted(&error);
        }
    }
}
