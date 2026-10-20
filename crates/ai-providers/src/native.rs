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
// exceed 90 seconds. Keep connection/discovery bounds and never retry a POST;
// only Gemini generation receives this longer, still bounded response window.
const GEMINI_GENERATION_TIMEOUT: Duration = Duration::from_secs(240);
const IMAGE_JSON_LIMIT: usize = 24 * 1024 * 1024;
const MAX_TOTAL_IMAGE_BYTES: usize = 32 * 1024 * 1024;

/// Clones share reqwest's connection pool. Credentials are attached only to requests, never to
/// this process-wide client; idle connections are capped per host and expire after a minute.
fn shared_client() -> Result<Client, String> {
    static CLIENT: OnceLock<Result<Client, &'static str>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            Client::builder()
                .connect_timeout(Duration::from_secs(10))
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
        let generation = self.provider == ProviderKind::Gemini && method == reqwest::Method::POST;
        let mut request = self.client.request(method, url);
        if generation {
            request = request.timeout(GEMINI_GENERATION_TIMEOUT);
        }
        if let Some(key) = self.key {
            let mut value = HeaderValue::from_str(if self.provider == ProviderKind::Gemini { key } else { "" }).map_err(|_| "Invalid API key")?;
            if self.provider != ProviderKind::Gemini {
                value = HeaderValue::from_str(&format!("Bearer {key}")).map_err(|_| "Invalid API key")?;
            }
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
}

fn send(request: RequestBuilder, cancel: &AtomicBool) -> Result<Response, String> {
    check_cancel(cancel)?;
    let response = request.send().map_err(|error| {
        // Do not expose reqwest errors: they can contain full URLs and credentials.
        if error.is_connect() {
            if error.is_timeout() {
                "AI provider connection timed out; no automatic retry was sent"
            } else {
                "AI provider connection failed; no automatic retry was sent"
            }
        } else if error.is_timeout() {
            "AI provider response timed out; request outcome is unknown; no automatic retry was sent"
        } else {
            "AI provider request failed before a complete response; request outcome is unknown; no automatic retry was sent"
        }
    })?;
    check_cancel(cancel)?;
    let status = response.status();
    if !status.is_success() {
        let detail = match status.as_u16() {
            401 | 403 => "authentication or permission denied",
            429 => "rate or quota limit reached",
            400 | 404 | 422 => "request, model, schema, or endpoint not supported",
            300..=399 => "redirect refused",
            _ => "provider returned an error",
        };
        // Error bodies may echo keys or confidential prompts. Never include them.
        return Err(format!("AI provider HTTP {}: {detail}", status.as_u16()));
    }
    Ok(response)
}

fn read_bounded(mut response: Response, cancel: &AtomicBool, limit: usize) -> Result<Vec<u8>, String> {
    if response.content_length().is_some_and(|size| size > limit as u64) {
        return Err("AI response exceeds size limit".into());
    }
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        check_cancel(cancel)?;
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
    check_cancel(cancel)?;
    Ok(bytes)
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
            json!({"systemInstruction":{"parts":[{"text":request.system}]},"contents":[{"role":"user","parts":[{"text":gemini_json_prompt(request)?}]}],
            "generationConfig":{"responseMimeType":"application/json","maxOutputTokens":request.max_output_tokens}})
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
    let body = generation_body(selection, request)?;
    let transport = Transport::new(selection, key)?;
    let path = match selection.provider {
        ProviderKind::OpenAi => "responses".into(),
        ProviderKind::Gemini => format!("models/{}:generateContent", model_id(selection)?),
        ProviderKind::Ollama => "api/chat".into(),
        ProviderKind::ComfyUi => return Err("ComfyUI requires an image workflow".into()),
    };
    let response = transport.post(&path, &body, cancel, MAX_JSON_BYTES)?;
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

pub(super) fn parse_generation(provider: ProviderKind, value: &Value) -> Result<GenerationResponse, String> {
    let usage = parse_usage(provider, value);
    let (text, error) = match generation_text(provider, value) {
        Ok(text) => (text, None),
        Err(error) => (String::new(), Some(error)),
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
            let candidate =
                value.get("candidates").and_then(Value::as_array).and_then(|v| v.first()).ok_or("Gemini returned no candidate (possibly blocked)")?;
            if candidate.get("finishReason").and_then(Value::as_str) != Some("STOP") {
                return Err("Gemini response did not finish normally".into());
            }
            candidate
                .pointer("/content/parts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|p| p.get("thought").and_then(Value::as_bool) != Some(true))
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<String>()
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
        // GPT Image models return base64 by default. No URL download is followed.
        let mut body = json!({"model":model,"prompt":request.prompt,"n":1});
        if let Some(size) = &request.size {
            body["size"] = json!(size);
        }
        if let Some(quality) = &request.quality {
            body["quality"] = json!(quality);
        }
        Ok(body)
    } else {
        let mut config = json!({"responseModalities":["TEXT","IMAGE"]});
        // Gemini's native image config uses an aspect ratio, not an OpenAI pixel size.
        // https://ai.google.dev/gemini-api/docs/image-generation
        if let Some(size) = &request.size {
            let (ratio, tier) = size.split_once('@').map_or((size.as_str(), None), |(ratio, tier)| (ratio, Some(tier)));
            if super::gemini_base_pixels(ratio).is_none() {
                return Err("Unsupported Gemini image aspect ratio".into());
            }
            config["imageConfig"] = json!({"aspectRatio":ratio});
            if let Some(tier) = tier {
                if !super::gemini_has_print_tiers(model) || !super::gemini_print_ratio_supported(model, ratio) || !matches!(tier, "1K" | "2K" | "4K")
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
        for candidate in value.get("candidates").and_then(Value::as_array).into_iter().flatten() {
            if candidate.get("finishReason").and_then(Value::as_str) != Some("STOP") {
                return Err("Gemini image response did not finish normally".into());
            }
            for part in candidate.pointer("/content/parts").and_then(Value::as_array).into_iter().flatten() {
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
    check_cancel(cancel)?;
    // Once submitted, capture its id even if cancellation arrives while waiting
    // for the response, so cleanup can address exactly this pending queue item.
    let submission = AtomicBool::new(false);
    let queued = transport.post("prompt", &json!({"prompt":request.workflow}), &submission, MAX_JSON_BYTES)?;
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
            let _ = send(request.timeout(Duration::from_secs(3)).json(&json!({"delete":[id]})), &cleanup);
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
        let mut remaining = request.poll_interval_ms;
        while remaining > 0 {
            check_cancel(cancel)?;
            let sleep = remaining.min(50);
            std::thread::sleep(Duration::from_millis(sleep));
            remaining = remaining.saturating_sub(sleep);
        }
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
