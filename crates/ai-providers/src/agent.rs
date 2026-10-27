use super::*;
use crate::native::{
    LocalOriginLease, Transport, encode_json, gemini_candidate_text, gemini_thinking_config, model_id, parse_usage, sanitize_reason_token,
    validate_value,
};
use base64::Engine;
use serde_json::json;

fn validate_tool_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 64 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
        return Err(format!("Invalid AI tool name '{name}'"));
    }
    Ok(())
}

pub(super) fn gemini_clean_tool_schema(schema: &Value, depth: usize) -> Result<Value, String> {
    if depth > 24 {
        return Err("AI tool parameter schema exceeds maximum nesting depth".into());
    }
    let Some(map) = schema.as_object() else {
        return Err("AI tool parameter schema node must be a JSON object".into());
    };
    let mut out = serde_json::Map::with_capacity(map.len());
    for (key, value) in map {
        if matches!(
            key.as_str(),
            "$schema" | "$defs" | "definitions" | "$ref" | "additionalProperties" | "minLength" | "maxLength" | "pattern" | "const"
        ) {
            continue;
        }
        match key.as_str() {
            "properties" => {
                let props = value.as_object().ok_or("AI tool schema properties must be an object")?;
                let mut clean_props = serde_json::Map::with_capacity(props.len());
                for (prop_name, prop_schema) in props {
                    clean_props.insert(prop_name.clone(), gemini_clean_tool_schema(prop_schema, depth.saturating_add(1))?);
                }
                out.insert(key.clone(), Value::Object(clean_props));
            }
            "items" => {
                if let Some(list) = value.as_array() {
                    let mut clean_list = Vec::with_capacity(list.len());
                    for item in list {
                        clean_list.push(gemini_clean_tool_schema(item, depth.saturating_add(1))?);
                    }
                    out.insert(key.clone(), Value::Array(clean_list));
                } else {
                    out.insert(key.clone(), gemini_clean_tool_schema(value, depth.saturating_add(1))?);
                }
            }
            "anyOf" | "oneOf" | "allOf" => {
                let list = value.as_array().ok_or("AI tool schema composite keyword must be an array")?;
                let mut clean_list = Vec::with_capacity(list.len());
                for item in list {
                    clean_list.push(gemini_clean_tool_schema(item, depth.saturating_add(1))?);
                }
                out.insert(key.clone(), Value::Array(clean_list));
            }
            _ => {
                out.insert(key.clone(), value.clone());
            }
        }
    }
    Ok(Value::Object(out))
}

fn openai_supports_encrypted_reasoning(model: &str) -> bool {
    let lower = model.trim().to_ascii_lowercase();
    let leaf = lower.rsplit('/').next().unwrap_or(lower.as_str());
    leaf.starts_with("o1") || leaf.starts_with("o3") || leaf.starts_with("o4") || leaf.starts_with("gpt-5")
}

fn sanitize_openai_stateless_item(item: &Value) -> Option<Value> {
    let obj = item.as_object()?;
    match obj.get("type").and_then(Value::as_str) {
        Some("reasoning") => {
            let enc = obj.get("encrypted_content").and_then(Value::as_str).filter(|s| !s.is_empty())?;
            let summary = obj.get("summary").cloned().unwrap_or_else(|| json!([]));
            Some(json!({
                "type": "reasoning",
                "summary": summary,
                "encrypted_content": enc,
            }))
        }
        Some("function_call") => {
            let mut clean = obj.clone();
            clean.remove("id");
            clean.remove("status");
            Some(Value::Object(clean))
        }
        Some("message") => {
            let mut clean = obj.clone();
            clean.remove("id");
            clean.remove("status");
            if !clean.contains_key("role") {
                clean.insert("role".into(), json!("assistant"));
            }
            Some(Value::Object(clean))
        }
        _ => None,
    }
}

fn gemini_supports_nested_function_response_parts(model: &str) -> bool {
    let lower = model.trim().to_ascii_lowercase();
    let leaf = lower.rsplit('/').next().unwrap_or(lower.as_str()).trim_start_matches("models/");
    leaf.starts_with("gemini-3")
}

fn gemini_has_nested_function_response_parts(body: &Value) -> bool {
    body.get("contents").and_then(Value::as_array).is_some_and(|contents| {
        contents.iter().any(|content| {
            content.get("parts").and_then(Value::as_array).is_some_and(|parts| {
                parts.iter().any(|part| {
                    part.get("functionResponse")
                        .and_then(Value::as_object)
                        .and_then(|fr| fr.get("parts"))
                        .and_then(Value::as_array)
                        .is_some_and(|nested| !nested.is_empty())
                })
            })
        })
    })
}

fn demote_gemini_nested_function_response_parts(body: &mut Value) -> bool {
    let mut changed = false;
    let Some(contents) = body.get_mut("contents").and_then(Value::as_array_mut) else {
        return false;
    };
    for content in contents {
        let Some(parts) = content.get_mut("parts").and_then(Value::as_array_mut) else {
            continue;
        };
        let mut trailing_media = Vec::new();
        for part in parts.iter_mut() {
            if let Some(fr) = part.get_mut("functionResponse").and_then(Value::as_object_mut)
                && let Some(Value::Array(nested)) = fr.remove("parts")
                && !nested.is_empty()
            {
                changed = true;
                trailing_media.extend(nested);
            }
        }
        if !trailing_media.is_empty() {
            parts.extend(trailing_media);
        }
    }
    changed
}

fn history_uses_ollama_json_fallback(request: &AgentTurnRequest) -> bool {
    request.messages.iter().any(|msg| {
        matches!(
            msg,
            AgentMessage::Assistant { tool_calls, .. } if tool_calls.iter().any(|tc| tc.id.starts_with("ollama_json_call_"))
        )
    })
}

struct EncodedMessageImages {
    user: Vec<String>,
    tool_results: Vec<Vec<String>>,
}

fn validate_agent_request(request: &AgentTurnRequest, include_tool_images: bool) -> Result<Vec<EncodedMessageImages>, String> {
    if request.messages.is_empty() || request.messages.len() > 128 {
        return Err("AI conversation history must contain between 1 and 128 messages".into());
    }
    if request.tools.len() > 64 {
        return Err("AI tool count exceeds limit".into());
    }
    if request.max_output_tokens == 0 || request.max_output_tokens > 131_072 {
        return Err("AI output token limit must be between 1 and 131072".into());
    }
    if request.system.len() > MAX_JSON_BYTES / 2 {
        return Err("AI system instructions exceed size limit".into());
    }
    for tool in &request.tools {
        validate_tool_name(&tool.name)?;
        if tool.description.len() > 8192 || !tool.parameters.is_object() {
            return Err(format!("AI tool '{}' has invalid description or non-object parameters", tool.name));
        }
        encode_json(&tool.parameters)?;
    }
    let mut total_images = 0usize;
    let mut total_image_bytes = 0usize;
    let mut encoded_per_msg = Vec::with_capacity(request.messages.len());
    for msg in &request.messages {
        match msg {
            AgentMessage::User { text, images } => {
                if text.len() > MAX_JSON_BYTES / 2 {
                    return Err("AI user message exceeds size limit".into());
                }
                total_images = total_images.saturating_add(images.len());
                if total_images > 8 {
                    return Err("AI reference image count exceeds limit".into());
                }
                let mut msg_encoded = Vec::with_capacity(images.len());
                for input in images {
                    if !matches!(input.mime_type.as_str(), "image/png" | "image/jpeg" | "image/webp") {
                        return Err("Unsupported AI reference image format".into());
                    }
                    total_image_bytes = total_image_bytes.saturating_add(input.bytes.len());
                    if input.bytes.is_empty() || total_image_bytes > 2 * 1024 * 1024 {
                        return Err("AI reference images exceed the 2 MiB combined limit or are empty".into());
                    }
                    msg_encoded.push(base64::engine::general_purpose::STANDARD.encode(&input.bytes));
                }
                encoded_per_msg.push(EncodedMessageImages { user: msg_encoded, tool_results: Vec::new() });
            }
            AgentMessage::Assistant { text, tool_calls, raw_provider_turn } => {
                if text.len() > MAX_JSON_BYTES / 2 || tool_calls.len() > 128 {
                    return Err("AI assistant message exceeds size limit".into());
                }
                for tc in tool_calls {
                    validate_tool_name(&tc.name)?;
                    validate_value(&tc.arguments, 0, &mut 0)?;
                }
                if let Some(raw) = raw_provider_turn {
                    validate_value(raw, 0, &mut 0)?;
                }
                encoded_per_msg.push(EncodedMessageImages { user: Vec::new(), tool_results: Vec::new() });
            }
            AgentMessage::ToolResults { results } => {
                if results.is_empty() || results.len() > 128 {
                    return Err("AI tool result batch must contain between 1 and 128 items".into());
                }
                let mut encoded_results = Vec::with_capacity(results.len());
                for res in results {
                    validate_tool_name(&res.name)?;
                    validate_value(&res.output, 0, &mut 0)?;
                    for input in &res.images {
                        if !matches!(input.mime_type.as_str(), "image/png" | "image/jpeg" | "image/webp") {
                            return Err("Unsupported AI tool render image format".into());
                        }
                        if input.bytes.is_empty() || input.bytes.len() > 2 * 1024 * 1024 {
                            return Err("AI tool render image exceeds size limit or is empty".into());
                        }
                    }
                    if include_tool_images && !res.images.is_empty() {
                        total_images = total_images.saturating_add(res.images.len());
                        if total_images > 8 {
                            return Err("AI multimodal image count exceeds limit".into());
                        }
                        let mut res_encoded = Vec::with_capacity(res.images.len());
                        for input in &res.images {
                            total_image_bytes = total_image_bytes.saturating_add(input.bytes.len());
                            if total_image_bytes > 2 * 1024 * 1024 {
                                return Err("AI multimodal images exceed the 2 MiB combined limit".into());
                            }
                            res_encoded.push(base64::engine::general_purpose::STANDARD.encode(&input.bytes));
                        }
                        encoded_results.push(res_encoded);
                    } else {
                        encoded_results.push(Vec::new());
                    }
                }
                encoded_per_msg.push(EncodedMessageImages { user: Vec::new(), tool_results: encoded_results });
            }
        }
    }
    Ok(encoded_per_msg)
}

pub(super) fn agent_turn_body(selection: &ModelSelection, request: &AgentTurnRequest, ollama_json_fallback: bool) -> Result<Value, String> {
    let model = model_id(selection)?;
    let include_tool_images = crate::supports_agent_vision(selection);
    let encoded_images = validate_agent_request(request, include_tool_images)?;
    let body = match selection.provider {
        ProviderKind::OpenAi => {
            let mut input = Vec::new();
            for (msg, msg_images) in request.messages.iter().zip(&encoded_images) {
                match msg {
                    AgentMessage::User { text, images } => {
                        if images.is_empty() {
                            input.push(json!({"role": "user", "content": text}));
                        } else {
                            let mut content = vec![json!({"type": "input_text", "text": text})];
                            for (img, encoded) in images.iter().zip(&msg_images.user) {
                                content.push(json!({"type": "input_image", "image_url": format!("data:{};base64,{encoded}", img.mime_type)}));
                            }
                            input.push(json!({"role": "user", "content": content}));
                        }
                    }
                    AgentMessage::Assistant { text, tool_calls, raw_provider_turn } => {
                        if let Some(Value::Array(items)) = raw_provider_turn {
                            let sanitized: Vec<Value> = items.iter().filter_map(sanitize_openai_stateless_item).collect();
                            if !sanitized.is_empty() {
                                input.extend(sanitized);
                            } else {
                                if !text.is_empty() {
                                    input.push(json!({"role": "assistant", "content": text}));
                                }
                                for tc in tool_calls {
                                    let args_str = serde_json::to_string(&tc.arguments).map_err(|_| "Cannot serialize tool call arguments")?;
                                    input.push(json!({
                                        "type": "function_call",
                                        "call_id": tc.id,
                                        "name": tc.name,
                                        "arguments": args_str,
                                    }));
                                }
                            }
                        } else {
                            if !text.is_empty() {
                                input.push(json!({"role": "assistant", "content": text}));
                            }
                            for tc in tool_calls {
                                let args_str = serde_json::to_string(&tc.arguments).map_err(|_| "Cannot serialize tool call arguments")?;
                                input.push(json!({
                                    "type": "function_call",
                                    "call_id": tc.id,
                                    "name": tc.name,
                                    "arguments": args_str,
                                }));
                            }
                        }
                    }
                    AgentMessage::ToolResults { results } => {
                        let mut visual_parts = Vec::new();
                        for (res, res_images) in results.iter().zip(&msg_images.tool_results) {
                            let out_str = serde_json::to_string(&res.output).map_err(|_| "Cannot serialize tool result")?;
                            input.push(json!({
                                "type": "function_call_output",
                                "call_id": res.call_id,
                                "output": out_str,
                            }));
                            for (img, encoded) in res.images.iter().zip(res_images) {
                                visual_parts.push((img.mime_type.as_str(), encoded.as_str()));
                            }
                        }
                        if !visual_parts.is_empty() {
                            let mut content = vec![json!({
                                "type": "input_text",
                                "text": "Visual render output attached from tool inspection:"
                            })];
                            for (mime, encoded) in visual_parts {
                                content.push(json!({
                                    "type": "input_image",
                                    "image_url": format!("data:{mime};base64,{encoded}"),
                                }));
                            }
                            input.push(json!({"role": "user", "content": content}));
                        }
                    }
                }
            }
            let mut body = json!({
                "model": model,
                "instructions": request.system,
                "input": input,
                "store": false,
                "max_output_tokens": request.max_output_tokens,
            });
            if openai_supports_encrypted_reasoning(model) {
                body["include"] = json!(["reasoning.encrypted_content"]);
            }
            if !request.tools.is_empty() {
                let tools: Vec<Value> = request
                    .tools
                    .iter()
                    .map(|t| {
                        json!({
                            "type": "function",
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                            "strict": false,
                        })
                    })
                    .collect();
                body["tools"] = Value::Array(tools);
            }
            body
        }
        ProviderKind::Gemini => {
            let mut contents = Vec::with_capacity(request.messages.len());
            for (msg, msg_images) in request.messages.iter().zip(&encoded_images) {
                match msg {
                    AgentMessage::User { text, images } => {
                        let mut parts = vec![json!({"text": text})];
                        for (img, encoded) in images.iter().zip(&msg_images.user) {
                            parts.push(json!({"inlineData": {"mimeType": img.mime_type, "data": encoded}}));
                        }
                        contents.push(json!({"role": "user", "parts": parts}));
                    }
                    AgentMessage::Assistant { text, tool_calls, raw_provider_turn } => {
                        if let Some(Value::Object(raw_map)) = raw_provider_turn
                            && raw_map.get("parts").is_some_and(Value::is_array)
                        {
                            let mut turn = raw_map.clone();
                            turn.insert("role".into(), json!("model"));
                            contents.push(Value::Object(turn));
                        } else {
                            let mut parts = Vec::new();
                            if !text.is_empty() {
                                parts.push(json!({"text": text}));
                            }
                            for tc in tool_calls {
                                let mut fc = serde_json::Map::new();
                                if !tc.id.is_empty() && !tc.id.starts_with("gemini_call_") {
                                    fc.insert("id".into(), json!(tc.id));
                                }
                                fc.insert("name".into(), json!(tc.name));
                                fc.insert("args".into(), tc.arguments.clone());
                                parts.push(json!({"functionCall": Value::Object(fc)}));
                            }
                            contents.push(json!({"role": "model", "parts": parts}));
                        }
                    }
                    AgentMessage::ToolResults { results } => {
                        let nest_media = gemini_supports_nested_function_response_parts(model);
                        let mut parts = Vec::with_capacity(results.len());
                        let mut trailing_media = Vec::new();
                        for (res, res_images) in results.iter().zip(&msg_images.tool_results) {
                            let response_obj = if res.output.is_object() { res.output.clone() } else { json!({"result": res.output}) };
                            let mut fr = serde_json::Map::new();
                            if !res.call_id.is_empty() && !res.call_id.starts_with("gemini_call_") {
                                fr.insert("id".into(), json!(res.call_id));
                            }
                            fr.insert("name".into(), json!(res.name));
                            fr.insert("response".into(), response_obj);
                            let media_parts: Vec<Value> = res
                                .images
                                .iter()
                                .zip(res_images)
                                .map(|(img, encoded)| json!({"inlineData": {"mimeType": img.mime_type, "data": encoded}}))
                                .collect();
                            if nest_media {
                                if !media_parts.is_empty() {
                                    fr.insert("parts".into(), Value::Array(media_parts));
                                }
                            } else {
                                trailing_media.extend(media_parts);
                            }
                            parts.push(json!({"functionResponse": Value::Object(fr)}));
                        }
                        if !trailing_media.is_empty() {
                            parts.extend(trailing_media);
                        }
                        contents.push(json!({"role": "user", "parts": parts}));
                    }
                }
            }
            let mut generation_config = json!({"maxOutputTokens": request.max_output_tokens});
            if let Some(thinking_config) = gemini_thinking_config(model, request.max_output_tokens)
                && let Some(cfg) = generation_config.as_object_mut()
            {
                cfg.insert("thinkingConfig".into(), thinking_config);
            }
            let mut body = json!({
                "systemInstruction": {"parts": [{"text": request.system}]},
                "contents": contents,
                "generationConfig": generation_config,
            });
            if !request.tools.is_empty() {
                let mut decls = Vec::with_capacity(request.tools.len());
                for t in &request.tools {
                    decls.push(json!({
                        "name": t.name,
                        "description": t.description,
                        "parameters": gemini_clean_tool_schema(&t.parameters, 0)?,
                    }));
                }
                body["tools"] = json!([{"functionDeclarations": decls}]);
            }
            body
        }
        ProviderKind::Ollama => {
            request.resources.validate()?;
            if request.max_output_tokens >= request.resources.context_tokens {
                return Err("Local output limit must be smaller than the context window".into());
            }
            let available = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
            let threads = usize::from(request.resources.cpu_threads).min(available.saturating_sub(1).max(1));
            let mut options = json!({
                "num_predict": request.max_output_tokens,
                "num_ctx": request.resources.context_tokens,
                "num_thread": threads,
                "num_batch": request.resources.batch_tokens,
            });
            if !request.resources.use_gpu {
                options["num_gpu"] = json!(0);
            }
            let mut system = request.system.clone();
            if ollama_json_fallback && !request.tools.is_empty() {
                let catalog: Vec<Value> =
                    request.tools.iter().map(|t| json!({"name": t.name, "description": t.description, "parameters": t.parameters})).collect();
                let catalog_str = serde_json::to_string(&catalog).map_err(|_| "Cannot encode fallback tool catalog")?;
                system.push_str(
                    "\n\nJSON TOOL CALL PROTOCOL (strict JSON response required):\n\
                    To invoke one or more tools, respond ONLY with JSON:\n\
                    {\"tool_calls\": [{\"name\": \"<tool_name>\", \"arguments\": { ... }}]}\n\
                    When all edits and checks are complete, respond ONLY with JSON:\n\
                    {\"final_response\": \"<concise summary>\"}\n\
                    AVAILABLE TOOLS:\n",
                );
                system.push_str(&catalog_str);
            }
            let mut messages = vec![json!({"role": "system", "content": system})];
            for (msg, msg_images) in request.messages.iter().zip(&encoded_images) {
                match msg {
                    AgentMessage::User { text, .. } => {
                        let mut m = json!({"role": "user", "content": text});
                        if !msg_images.user.is_empty() {
                            m["images"] = json!(msg_images.user);
                        }
                        messages.push(m);
                    }
                    AgentMessage::Assistant { text, tool_calls, .. } => {
                        if ollama_json_fallback {
                            let content = if !tool_calls.is_empty() {
                                let calls: Vec<Value> = tool_calls.iter().map(|tc| json!({"name": tc.name, "arguments": tc.arguments})).collect();
                                serde_json::to_string(&json!({"tool_calls": calls})).unwrap_or_else(|_| text.clone())
                            } else {
                                text.clone()
                            };
                            messages.push(json!({"role": "assistant", "content": content}));
                        } else {
                            let mut m = json!({"role": "assistant", "content": text});
                            if !tool_calls.is_empty() {
                                let calls: Vec<Value> = tool_calls
                                    .iter()
                                    .map(|tc| json!({"type": "function", "function": {"name": tc.name, "arguments": tc.arguments}}))
                                    .collect();
                                m["tool_calls"] = Value::Array(calls);
                            }
                            messages.push(m);
                        }
                    }
                    AgentMessage::ToolResults { results } => {
                        let batch_images: Vec<&String> = msg_images.tool_results.iter().flat_map(|v| v.iter()).collect();
                        if ollama_json_fallback {
                            let payload: Vec<Value> =
                                results.iter().map(|r| json!({"tool": r.name, "call_id": r.call_id, "output": r.output})).collect();
                            let content = serde_json::to_string(&json!({"tool_results": payload})).map_err(|_| "Cannot encode tool results")?;
                            let mut m = json!({"role": "user", "content": content});
                            if !batch_images.is_empty() {
                                m["images"] = json!(batch_images);
                            }
                            messages.push(m);
                        } else {
                            for res in results {
                                let content = serde_json::to_string(&res.output).map_err(|_| "Cannot encode tool result")?;
                                messages.push(json!({
                                    "role": "tool",
                                    "tool_name": res.name,
                                    "content": content,
                                }));
                            }
                            if !batch_images.is_empty() {
                                messages.push(json!({
                                    "role": "user",
                                    "content": "Visual render output attached from tool inspection.",
                                    "images": batch_images,
                                }));
                            }
                        }
                    }
                }
            }
            let mut body = json!({
                "model": model,
                "stream": false,
                "messages": messages,
                "options": options,
                "keep_alive": request.resources.keep_alive_seconds,
            });
            if ollama_json_fallback {
                body["format"] = json!("json");
            } else if !request.tools.is_empty() {
                let tools: Vec<Value> = request
                    .tools
                    .iter()
                    .map(|t| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": t.name,
                                "description": t.description,
                                "parameters": t.parameters,
                            }
                        })
                    })
                    .collect();
                body["tools"] = Value::Array(tools);
            }
            body
        }
        ProviderKind::ComfyUi => return Err("ComfyUI supports image workflows, not agent tool turns".into()),
    };
    encode_json(&body)?;
    Ok(body)
}

pub(super) fn generate_agent_turn(
    selection: &ModelSelection,
    key: Option<&str>,
    request: &AgentTurnRequest,
    cancel: &AtomicBool,
) -> Result<AgentTurnResponse, String> {
    let transport = Transport::new(selection, key)?;
    let _origin_lease = LocalOriginLease::acquire(transport.provider, &transport.base)?;
    generate_agent_turn_with_post(selection, request, cancel, |path, body, once| {
        if once { transport.post_once(path, body, cancel, MAX_JSON_BYTES) } else { transport.post(path, body, cancel, MAX_JSON_BYTES) }
    })
}

pub(super) fn generate_agent_turn_with_post(
    selection: &ModelSelection,
    request: &AgentTurnRequest,
    cancel: &AtomicBool,
    mut post: impl FnMut(&str, &Value, bool) -> Result<Value, String>,
) -> Result<AgentTurnResponse, String> {
    let initial_ollama_fallback = selection.provider == ProviderKind::Ollama && history_uses_ollama_json_fallback(request);
    let body = agent_turn_body(selection, request, initial_ollama_fallback)?;
    let path = match selection.provider {
        ProviderKind::OpenAi => "responses".into(),
        ProviderKind::Gemini => format!("models/{}:generateContent", model_id(selection)?),
        ProviderKind::Ollama => "api/chat".into(),
        ProviderKind::ComfyUi => return Err("ComfyUI requires an image workflow".into()),
    };
    let response = match post(&path, &body, false) {
        Ok(value) => value,
        Err(error) if selection.provider == ProviderKind::OpenAi && error.starts_with("AI provider HTTP 400:") && body.get("include").is_some() => {
            crate::check_cancel(cancel)?;
            let mut fallback_body = body;
            if let Some(obj) = fallback_body.as_object_mut() {
                obj.remove("include");
            }
            post(&path, &fallback_body, true)?
        }
        Err(error)
            if selection.provider == ProviderKind::Gemini
                && error.starts_with("AI provider HTTP 400:")
                && (body.get("generationConfig").and_then(Value::as_object).is_some_and(|cfg| cfg.contains_key("thinkingConfig"))
                    || gemini_has_nested_function_response_parts(&body)) =>
        {
            crate::check_cancel(cancel)?;
            let mut fallback_body = body;
            let _ = demote_gemini_nested_function_response_parts(&mut fallback_body);
            if let Some(cfg) = fallback_body.get_mut("generationConfig").and_then(Value::as_object_mut) {
                cfg.remove("thinkingConfig");
            }
            post(&path, &fallback_body, true)?
        }
        Err(error)
            if selection.provider == ProviderKind::Ollama
                && !initial_ollama_fallback
                && (error.starts_with("AI provider HTTP 400:") || error.starts_with("AI provider HTTP 422:"))
                && !request.tools.is_empty() =>
        {
            crate::check_cancel(cancel)?;
            let fallback_body = agent_turn_body(selection, request, true)?;
            post(&path, &fallback_body, true)?
        }
        Err(error) => return Err(error),
    };
    parse_agent_turn(selection.provider, &response)
}

pub(super) fn parse_agent_turn(provider: ProviderKind, value: &Value) -> Result<AgentTurnResponse, String> {
    let usage = parse_usage(provider, value);
    if value.get("error").is_some_and(|v| !v.is_null()) {
        return Ok(AgentTurnResponse {
            text: String::new(),
            tool_calls: Vec::new(),
            raw_provider_turn: None,
            usage,
            error: Some("AI provider returned an error".into()),
        });
    }
    match provider {
        ProviderKind::OpenAi => {
            if value.get("status").and_then(Value::as_str) != Some("completed") {
                return Ok(AgentTurnResponse {
                    text: String::new(),
                    tool_calls: Vec::new(),
                    raw_provider_turn: None,
                    usage,
                    error: Some("AI response did not complete (refused, interrupted, or output limit reached)".into()),
                });
            }
            let mut text = String::new();
            let mut tool_calls = Vec::new();
            let output_items = value.get("output").and_then(Value::as_array);
            for (idx, item) in output_items.into_iter().flatten().enumerate() {
                match item.get("type").and_then(Value::as_str) {
                    Some("message") => {
                        for part in item.get("content").and_then(Value::as_array).into_iter().flatten() {
                            if part.get("type").and_then(Value::as_str) == Some("refusal") {
                                return Ok(AgentTurnResponse {
                                    text: String::new(),
                                    tool_calls: Vec::new(),
                                    raw_provider_turn: None,
                                    usage,
                                    error: Some("AI provider refused the request".into()),
                                });
                            }
                            if part.get("type").and_then(Value::as_str) == Some("output_text")
                                && let Some(chunk) = part.get("text").and_then(Value::as_str)
                            {
                                text.push_str(chunk);
                            }
                        }
                    }
                    Some("function_call") => {
                        let Some(name) = item.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
                            return Ok(AgentTurnResponse {
                                text,
                                tool_calls: Vec::new(),
                                raw_provider_turn: None,
                                usage,
                                error: Some("OpenAI function_call missing name".into()),
                            });
                        };
                        let call_id = item
                            .get("call_id")
                            .or_else(|| item.get("id"))
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("openai_call_{idx}"));
                        let raw_args = item.get("arguments").and_then(Value::as_str).unwrap_or("{}");
                        let trimmed = raw_args.trim();
                        let arguments = if trimmed.is_empty() {
                            json!({})
                        } else {
                            match serde_json::from_str::<Value>(trimmed) {
                                Ok(parsed) => parsed,
                                Err(_) => {
                                    return Ok(AgentTurnResponse {
                                        text,
                                        tool_calls: Vec::new(),
                                        raw_provider_turn: None,
                                        usage,
                                        error: Some(format!("OpenAI function_call '{name}' returned malformed JSON arguments")),
                                    });
                                }
                            }
                        };
                        tool_calls.push(ToolCall { id: call_id, name: name.to_string(), arguments });
                    }
                    _ => {}
                }
            }
            if text.len() > MAX_JSON_BYTES || (text.is_empty() && tool_calls.is_empty()) {
                return Ok(AgentTurnResponse {
                    text: String::new(),
                    tool_calls: Vec::new(),
                    raw_provider_turn: None,
                    usage,
                    error: Some("AI provider returned empty or excessive agent response".into()),
                });
            }
            Ok(AgentTurnResponse { text, tool_calls, raw_provider_turn: output_items.map(|items| Value::Array(items.clone())), usage, error: None })
        }
        ProviderKind::Gemini => {
            let Some(candidate) = value.get("candidates").and_then(Value::as_array).and_then(|v| v.first()) else {
                let block_reason = sanitize_reason_token(value.pointer("/promptFeedback/blockReason").and_then(Value::as_str));
                let err = match block_reason {
                    Some(reason) => format!("Gemini returned no candidate ({reason})"),
                    None => "Gemini returned no candidate (possibly blocked)".into(),
                };
                return Ok(AgentTurnResponse { text: String::new(), tool_calls: Vec::new(), raw_provider_turn: None, usage, error: Some(err) });
            };
            let finish_reason = candidate.get("finishReason").and_then(Value::as_str);
            if matches!(finish_reason, Some("MALFORMED_FUNCTION_CALL" | "UNEXPECTED_TOOL_CALL")) {
                let reason = finish_reason.unwrap_or("TOOL_ERROR");
                return Ok(AgentTurnResponse {
                    text: String::new(),
                    tool_calls: Vec::new(),
                    raw_provider_turn: None,
                    usage,
                    error: Some(format!("Gemini tool call failed ({reason})")),
                });
            }
            let text = gemini_candidate_text(candidate);
            let mut tool_calls = Vec::new();
            for (idx, part) in candidate.pointer("/content/parts").and_then(Value::as_array).into_iter().flatten().enumerate() {
                if let Some(fc) = part.get("functionCall") {
                    let Some(name) = fc.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
                        return Ok(AgentTurnResponse {
                            text,
                            tool_calls: Vec::new(),
                            raw_provider_turn: None,
                            usage,
                            error: Some("Gemini functionCall missing name".into()),
                        });
                    };
                    let id = fc
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("gemini_call_{idx}"));
                    let arguments = fc.get("args").cloned().filter(|v| !v.is_null()).unwrap_or_else(|| json!({}));
                    tool_calls.push(ToolCall { id, name: name.to_string(), arguments });
                }
            }
            if tool_calls.is_empty() && !matches!(finish_reason, Some("STOP") | None) {
                let err = match sanitize_reason_token(finish_reason) {
                    Some(reason) => format!("Gemini response did not finish normally ({reason})"),
                    None => "Gemini response did not finish normally".into(),
                };
                let partial = if text.len() <= MAX_JSON_BYTES { text } else { String::new() };
                return Ok(AgentTurnResponse { text: partial, tool_calls: Vec::new(), raw_provider_turn: None, usage, error: Some(err) });
            }
            if text.len() > MAX_JSON_BYTES || (text.is_empty() && tool_calls.is_empty()) {
                return Ok(AgentTurnResponse {
                    text: String::new(),
                    tool_calls: Vec::new(),
                    raw_provider_turn: None,
                    usage,
                    error: Some("AI provider returned empty or excessive agent response".into()),
                });
            }
            Ok(AgentTurnResponse { text, tool_calls, raw_provider_turn: candidate.get("content").cloned(), usage, error: None })
        }
        ProviderKind::Ollama => {
            if value.get("done").and_then(Value::as_bool) != Some(true) || value.get("done_reason").and_then(Value::as_str) == Some("length") {
                return Ok(AgentTurnResponse {
                    text: String::new(),
                    tool_calls: Vec::new(),
                    raw_provider_turn: None,
                    usage,
                    error: Some("Ollama response incomplete or output limit reached".into()),
                });
            }
            let message = value.get("message");
            let mut text = message.and_then(|m| m.get("content")).and_then(Value::as_str).unwrap_or("").to_string();
            let mut tool_calls = Vec::new();
            for (idx, tc) in message.and_then(|m| m.get("tool_calls")).and_then(Value::as_array).into_iter().flatten().enumerate() {
                let func = tc.get("function").unwrap_or(tc);
                let Some(name) = func.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
                    continue;
                };
                let id = tc
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("ollama_call_{idx}"));
                let arguments = match func.get("arguments") {
                    Some(Value::String(raw)) => {
                        let trimmed = raw.trim();
                        if trimmed.is_empty() {
                            json!({})
                        } else {
                            match serde_json::from_str::<Value>(trimmed) {
                                Ok(parsed) => parsed,
                                Err(_) => {
                                    return Ok(AgentTurnResponse {
                                        text,
                                        tool_calls: Vec::new(),
                                        raw_provider_turn: None,
                                        usage,
                                        error: Some(format!("Ollama tool '{name}' returned malformed JSON arguments")),
                                    });
                                }
                            }
                        }
                    }
                    Some(v) if !v.is_null() => v.clone(),
                    _ => json!({}),
                };
                tool_calls.push(ToolCall { id, name: name.to_string(), arguments });
            }
            if tool_calls.is_empty()
                && let Some(envelope) = extract_json_envelope(&text)
            {
                if let Some(calls) = envelope.get("tool_calls").and_then(Value::as_array) {
                    for (idx, tc) in calls.iter().enumerate() {
                        let func = tc.get("function").unwrap_or(tc);
                        if let Some(name) = func.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                            let arguments =
                                func.get("arguments").or_else(|| func.get("args")).cloned().filter(|v| !v.is_null()).unwrap_or_else(|| json!({}));
                            tool_calls.push(ToolCall { id: format!("ollama_json_call_{idx}"), name: name.to_string(), arguments });
                        }
                    }
                    if !tool_calls.is_empty() {
                        text = envelope.get("thought").and_then(Value::as_str).unwrap_or("").to_string();
                    }
                } else if let Some(final_text) = envelope.get("final_response").and_then(Value::as_str) {
                    text = final_text.to_string();
                }
            }
            if text.len() > MAX_JSON_BYTES || (text.is_empty() && tool_calls.is_empty()) {
                return Ok(AgentTurnResponse {
                    text: String::new(),
                    tool_calls: Vec::new(),
                    raw_provider_turn: None,
                    usage,
                    error: Some("Ollama returned empty or excessive agent response".into()),
                });
            }
            Ok(AgentTurnResponse { text, tool_calls, raw_provider_turn: None, usage, error: None })
        }
        ProviderKind::ComfyUi => Err("ComfyUI does not provide agent tool turns".into()),
    }
}

fn extract_json_envelope(raw: &str) -> Option<Value> {
    let trimmed = raw.trim();
    if let Ok(val) = serde_json::from_str::<Value>(trimmed)
        && val.is_object()
    {
        return Some(val);
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    if end > start
        && let Some(slice) = trimmed.get(start..=end)
        && let Ok(val) = serde_json::from_str::<Value>(slice)
        && val.is_object()
    {
        return Some(val);
    }
    None
}
