#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use super::*;
use serde_json::json;

fn selection(provider: ProviderKind, endpoint: Option<&str>) -> ModelSelection {
    ModelSelection { provider, model: "test-model".into(), endpoint: endpoint.map(str::to_string) }
}

#[test]
fn endpoint_security() {
    for endpoint in
        ["http://api.openai.com/v1", "https://key@example.org", "https://example.org/?key=secret", "https://example.org/#secret", "file:///secret"]
    {
        assert!(validate_endpoint(&selection(ProviderKind::OpenAi, Some(endpoint))).is_err());
    }
    for endpoint in ["http://localhost:11434", "http://127.0.0.1:11434", "http://[::1]:11434"] {
        assert!(validate_endpoint(&selection(ProviderKind::Ollama, Some(endpoint))).is_ok());
    }
    for endpoint in ["http://192.168.1.1", "http://localhost.example.org", "http://0.0.0.0"] {
        assert!(validate_endpoint(&selection(ProviderKind::Ollama, Some(endpoint))).is_err());
    }
    assert_eq!(validate_endpoint(&selection(ProviderKind::OpenAi, None)).unwrap().as_str(), "https://api.openai.com/v1/");
}

#[test]
fn secret_debug_is_redacted() {
    let secret = SecretString::new("test-key-not-real".into());
    assert!(!format!("{secret:?}").contains(secret.expose()));
}

#[test]
fn pre_cancel_never_contacts_provider() {
    let cancel = AtomicBool::new(true);
    assert_eq!(list_models(&selection(ProviderKind::Ollama, None), None, &cancel).unwrap_err(), "AI request cancelled");
}

#[cfg(not(target_arch = "wasm32"))]
mod native_tests {
    use super::*;
    use crate::native::{generation_body, parse_generation};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread::JoinHandle;

    fn request() -> GenerationRequest {
        GenerationRequest {
            system: "Create editable native commands".into(),
            prompt: "A page".into(),
            json_schema: json!({"type":"object","properties":{"actions":{"type":"array"}}}),
            max_output_tokens: 256,
            images: Vec::new(),
            resources: Default::default(),
        }
    }

    #[test]
    fn gemini_image_aspect_ratio_is_sent_in_the_native_request() {
        let request = ImageRequest {
            prompt: "Four isolated plant growth stages".into(),
            size: Some("4:1".into()),
            quality: None,
            comfy: None,
            resources: Default::default(),
        };
        let body = crate::native::image_generation_body(ProviderKind::Gemini, "test-model", &request).unwrap();
        assert_eq!(body["generationConfig"]["imageConfig"]["aspectRatio"], "4:1");
        let invalid = ImageRequest { size: Some("not-a-ratio".into()), ..request };
        assert!(crate::native::image_generation_body(ProviderKind::Gemini, "test-model", &invalid).unwrap_err().contains("aspect ratio"));
    }

    #[test]
    fn gemini_print_resolution_uses_smallest_supported_native_tier() {
        assert_eq!(gemini_print_size("gemini-3.1-flash-image", "1:1", [1600, 1000]).unwrap(), "1:1@2K");
        assert_eq!(gemini_print_size("gemini-3.1-flash-image", "4:1", [2800, 700]).unwrap(), "4:1@2K");
        assert_eq!(gemini_print_size("gemini-3.1-flash-image", "1:1", [2050, 2050]).unwrap(), "1:1@4K");
        assert!(gemini_print_size("gemini-3.1-flash-image", "1:1", [4097, 100]).unwrap_err().contains("4K"));
        assert!(gemini_print_size("gemini-2.5-flash-image", "1:1", [1600, 1000]).unwrap_err().contains("model"));
        assert!(gemini_print_size("gemini-3.1-flash-lite-image", "1:1", [1600, 1000]).is_err());
        assert!(gemini_print_size("gemini-3.1-flash-image", "1:0", [1, 1]).is_err());
        assert!(gemini_print_size("gemini-3.1-flash-image", "1:1", [0, 1]).is_err());
        let request = ImageRequest {
            prompt: "Original synthetic illustration".into(),
            size: Some("4:1@2K".into()),
            quality: None,
            comfy: None,
            resources: Default::default(),
        };
        let body = crate::native::image_generation_body(ProviderKind::Gemini, "gemini-3.1-flash-image", &request).unwrap();
        assert_eq!(body["generationConfig"]["imageConfig"], json!({"aspectRatio":"4:1","imageSize":"2K"}));
        for size in ["1:1@2k", "1:1@8K", "1:1@2K@4K"] {
            let invalid = ImageRequest { size: Some(size.into()), ..request.clone() };
            assert!(crate::native::image_generation_body(ProviderKind::Gemini, "gemini-3.1-flash-image", &invalid).is_err());
        }
        assert!(crate::native::image_generation_body(ProviderKind::Gemini, "gemini-2.5-flash-image", &request).is_err());
    }

    /// Minimal synthetic server captures complete request and returns fixed data.
    fn mock(responses: Vec<(u16, String)>) -> (String, JoinHandle<Vec<String>>) {
        mock_cancellable(responses, None)
    }

    fn mock_cancellable(responses: Vec<(u16, String)>, cancel: Option<std::sync::Arc<AtomicBool>>) -> (String, JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            let mut captured = Vec::new();
            for (status, body) in responses {
                let (mut socket, _) = listener.accept().unwrap();
                socket.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let count = socket.read(&mut chunk).unwrap();
                    if count == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&chunk[..count]);
                    let text = String::from_utf8_lossy(&bytes);
                    if let Some((headers, body)) = text.split_once("\r\n\r\n") {
                        let length = headers
                            .lines()
                            .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|s| s.trim().parse::<usize>().ok()))
                            .unwrap_or(0);
                        if body.len() >= length {
                            break;
                        }
                    }
                }
                captured.push(String::from_utf8(bytes).unwrap());
                if captured.len() == 1
                    && let Some(cancel) = &cancel
                {
                    cancel.store(true, Ordering::Relaxed);
                }
                let _ = write!(
                    socket,
                    "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
            captured
        });
        (endpoint, worker)
    }

    #[test]
    fn gemini_layout_json_mode_keeps_the_native_contract_without_a_schema_endpoint_requirement() {
        let mut request = request();
        request.prompt = "أنشئ صفحات أصلية قابلة للتحرير".into();
        request.json_schema = json!({"type":"object","required":["title","commands","images"],"properties":{
            "title":{"type":"string"},
            "commands":{"type":"array","items":{"type":"object","properties":{
                "command":{"type":"string","enum":["frame.create","type.char"]},"params":{"type":"object"}}}},
            "images":{"type":"array","maxItems":0,"items":{"type":"object"}}
        }});
        let body = generation_body(&selection(ProviderKind::Gemini, None), &request).unwrap();
        assert_eq!(body.pointer("/generationConfig/responseMimeType"), Some(&json!("application/json")));
        assert!(
            body.pointer("/generationConfig/responseJsonSchema").is_none(),
            "Observed Gemini endpoint rejects the layout schema before generation"
        );
        let prompt = body.pointer("/contents/0/parts/0/text").and_then(Value::as_str).unwrap();
        assert!(prompt.starts_with(&request.prompt));
        let contract = prompt.split_once("OUTPUT JSON CONTRACT:\n").unwrap().1;
        assert_eq!(serde_json::from_str::<Value>(contract).unwrap(), request.json_schema);
        assert_eq!(body.pointer("/generationConfig/maxOutputTokens"), Some(&json!(256)));
        assert_eq!(body.pointer("/systemInstruction/parts/0/text"), Some(&json!(request.system)));
    }

    #[test]
    fn gemini_reference_images_preserve_the_json_contract() {
        let mut request = request();
        request.images.push(ImageInput { mime_type: "image/png".into(), bytes: vec![1, 2, 3] });
        let body = generation_body(&selection(ProviderKind::Gemini, None), &request).unwrap();
        let prompt = body.pointer("/contents/0/parts/0/text").and_then(Value::as_str).unwrap();
        assert!(prompt.contains("OUTPUT JSON CONTRACT:\n"));
        assert_eq!(body.pointer("/contents/0/parts/1/inlineData/data"), Some(&json!("AQID")));
    }

    #[test]
    fn actual_provider_request_shapes() {
        let request = request();
        let openai = generation_body(&selection(ProviderKind::OpenAi, None), &request).unwrap();
        assert_eq!(openai.pointer("/text/format/type"), Some(&json!("json_schema")));
        assert_eq!(openai.pointer("/text/format/strict"), Some(&json!(false)));
        assert_eq!(openai.get("store"), Some(&json!(false)));
        let gemini = generation_body(&selection(ProviderKind::Gemini, None), &request).unwrap();
        assert!(gemini.pointer("/generationConfig/responseJsonSchema").is_none());
        assert!(gemini.pointer("/contents/0/parts/0/text").and_then(Value::as_str).unwrap().contains("OUTPUT JSON CONTRACT:\n"));
        let ollama = generation_body(&selection(ProviderKind::Ollama, None), &request).unwrap();
        assert_eq!(ollama.get("format"), Some(&request.json_schema));
        assert_eq!(ollama.get("stream"), Some(&json!(false)));
        assert_eq!(ollama.pointer("/options/num_predict"), Some(&json!(256)));
    }

    #[test]
    fn local_policy_limits_reach_ollama_without_leaking_into_cloud_requests() {
        let mut request = request();
        request.resources.cpu_threads = 2;
        request.resources.context_tokens = 4096;
        request.resources.batch_tokens = 32;
        request.resources.use_gpu = false;
        request.resources.keep_alive_seconds = 0;
        let local = selection(ProviderKind::Ollama, None);
        let body = generation_body(&local, &request).unwrap();
        assert_eq!(body.pointer("/options/num_gpu"), Some(&json!(0)));
        assert_eq!(body.pointer("/options/num_ctx"), Some(&json!(4096)));
        assert_eq!(body.pointer("/options/num_batch"), Some(&json!(32)));
        assert!((1..=2).contains(&body.pointer("/options/num_thread").and_then(Value::as_u64).unwrap()));
        assert_eq!(body["keep_alive"], 0);
        for cloud in [ProviderKind::OpenAi, ProviderKind::Gemini] {
            let cloud = generation_body(&selection(cloud, None), &request).unwrap();
            assert!(cloud.get("options").is_none());
            assert!(cloud.get("keep_alive").is_none());
        }
        request.max_output_tokens = 4096;
        assert!(generation_body(&local, &request).is_err());
    }

    #[test]
    fn vision_reference_shapes() {
        let mut request = request();
        request.images.push(ImageInput { mime_type: "image/png".into(), bytes: vec![1, 2, 3] });
        let openai = generation_body(&selection(ProviderKind::OpenAi, None), &request).unwrap();
        assert_eq!(openai.pointer("/input/0/content/1/image_url"), Some(&json!("data:image/png;base64,AQID")));
        let gemini = generation_body(&selection(ProviderKind::Gemini, None), &request).unwrap();
        assert_eq!(gemini.pointer("/contents/0/parts/1/inlineData/data"), Some(&json!("AQID")));
        let ollama = generation_body(&selection(ProviderKind::Ollama, None), &request).unwrap();
        assert_eq!(ollama.pointer("/messages/1/images/0"), Some(&json!("AQID")));
    }

    #[test]
    fn image_response_fixtures() {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(b"RIFFxxxxWEBPtest");
        let openai = json!({"data":[{"b64_json":encoded}],"usage":{"input_tokens":42,"output_tokens":100}});
        let cancel = AtomicBool::new(false);
        let response = crate::native::parse_images(ProviderKind::OpenAi, &openai, &cancel).unwrap();
        assert_eq!(response.images[0].mime_type, "image/webp");
        assert_eq!(response.images[0].filename, "generated-1.webp");
        assert_eq!(response.usage.input_tokens, Some(42));
        let gemini = json!({"candidates":[{"finishReason":"STOP","content":{"parts":[{"inlineData":{"mimeType":"image/webp","data":encoded}}]}}]});
        assert_eq!(crate::native::parse_images(ProviderKind::Gemini, &gemini, &cancel).unwrap().images.len(), 1);
        let invalid = json!({"data":[{"b64_json":"not-valid!"}]});
        assert!(crate::native::parse_images(ProviderKind::OpenAi, &invalid, &cancel).unwrap().error.is_some());
        assert!(
            crate::native::parse_images(ProviderKind::OpenAi, &json!({"data":[{"url":"https://untrusted.example"}]}), &cancel)
                .unwrap()
                .error
                .is_some()
        );
    }

    #[test]
    fn ai21_gemini_image_thoughts_do_not_become_placement_images() {
        use base64::Engine;
        let thought = base64::engine::general_purpose::STANDARD.encode(b"RIFFxxxxWEBPthought");
        let final_bytes = b"RIFFxxxxWEBPfinal";
        let final_image = base64::engine::general_purpose::STANDARD.encode(final_bytes);
        let value = json!({
            "candidates":[{"finishReason":"STOP","content":{"parts":[
                {"thought":true,"inlineData":{"mimeType":"image/webp","data":thought}},
                {"text":"Final illustration"},
                {"inlineData":{"mimeType":"image/webp","data":final_image}}
            ]}}],
            "usageMetadata":{"promptTokenCount":42,"candidatesTokenCount":100,"thoughtsTokenCount":17,"cachedContentTokenCount":8}
        });
        let response = crate::native::parse_images(ProviderKind::Gemini, &value, &AtomicBool::new(false)).unwrap();
        assert!(response.error.is_none());
        assert_eq!(response.images.len(), 1, "Thought images must not count as final placement images");
        assert_eq!(response.images[0].bytes, final_bytes);
        assert_eq!(response.images[0].filename, "generated-1.webp");
        assert_eq!(response.usage.input_tokens, Some(42));
        assert_eq!(response.usage.output_tokens, Some(117));
        assert_eq!(response.usage.reasoning_tokens, Some(17));
        assert_eq!(response.usage.cached_input_tokens, Some(8));
    }

    #[test]
    fn ai21_gemini_malformed_image_thoughts_do_not_discard_final_image() {
        use base64::Engine;
        let final_bytes = b"RIFFxxxxWEBPfinal";
        let final_image = base64::engine::general_purpose::STANDARD.encode(final_bytes);
        for inline_data in [
            json!({"mimeType":"image/webp","data":"not-valid!"}),
            json!({"mimeType":"image/webp","data":"AQID"}),
            json!({"mimeType":"image/webp","data":17}),
            json!({}),
        ] {
            let value = json!({
                "candidates":[{"finishReason":"STOP","content":{"parts":[
                    {"thought":true,"inlineData":inline_data},
                    {"inlineData":{"mimeType":"image/webp","data":final_image}}
                ]}}],
                "usageMetadata":{"promptTokenCount":9,"candidatesTokenCount":12,"thoughtsTokenCount":3}
            });
            let response = crate::native::parse_images(ProviderKind::Gemini, &value, &AtomicBool::new(false)).unwrap();
            assert!(response.error.is_none(), "Thought blobs must be skipped before decoding: {:?}", response.error);
            assert_eq!(response.images.len(), 1);
            assert_eq!(response.images[0].bytes, final_bytes);
            assert_eq!(response.usage.input_tokens, Some(9));
            assert_eq!(response.usage.output_tokens, Some(15));
            assert_eq!(response.usage.reasoning_tokens, Some(3));
        }
    }

    #[test]
    fn ai21_gemini_thought_only_images_fail_without_losing_measured_usage() {
        use base64::Engine;
        let thought = base64::engine::general_purpose::STANDARD.encode(b"RIFFxxxxWEBPthought");
        for data in [thought.as_str(), "not-valid!"] {
            let value = json!({
                "candidates":[{"finishReason":"STOP","content":{"parts":[
                    {"thought":true,"inlineData":{"mimeType":"image/webp","data":data}}
                ]}}],
                "usageMetadata":{"promptTokenCount":6,"candidatesTokenCount":0,"thoughtsTokenCount":11}
            });
            let response = crate::native::parse_images(ProviderKind::Gemini, &value, &AtomicBool::new(false)).unwrap();
            assert!(response.images.is_empty(), "A thought cannot satisfy the requested final illustration");
            assert_eq!(response.error.as_deref(), Some("Selected model returned no generated images"));
            assert_eq!(response.usage.input_tokens, Some(6));
            assert_eq!(response.usage.output_tokens, Some(11));
            assert_eq!(response.usage.reasoning_tokens, Some(11));
        }
    }

    #[test]
    fn ai21_gemini_multiple_final_images_remain_visible_to_host_validation() {
        use base64::Engine;
        let first_bytes = b"RIFFxxxxWEBPfirst";
        let second_bytes = b"RIFFxxxxWEBPsecond";
        let first = base64::engine::general_purpose::STANDARD.encode(first_bytes);
        let second = base64::engine::general_purpose::STANDARD.encode(second_bytes);
        let value = json!({"candidates":[{"finishReason":"STOP","content":{"parts":[
            {"inlineData":{"mimeType":"image/webp","data":first}},
            {"thought":false,"inlineData":{"mimeType":"image/webp","data":second}}
        ]}}]});
        let response = crate::native::parse_images(ProviderKind::Gemini, &value, &AtomicBool::new(false)).unwrap();
        assert!(response.error.is_none());
        assert_eq!(response.images.len(), 2, "The adapter must not silently select one of multiple final images");
        assert_eq!(response.images[0].bytes, first_bytes);
        assert_eq!(response.images[1].bytes, second_bytes);
        assert_eq!(response.images[0].filename, "generated-1.webp");
        assert_eq!(response.images[1].filename, "generated-2.webp");
    }

    #[test]
    fn image_failure_preserves_measured_provider_usage() {
        let cancel = AtomicBool::new(false);
        let openai = json!({"data":[{"b64_json":"not-valid!"}],"usage":{"input_tokens":42,"output_tokens":100}});
        let response = crate::native::parse_images(ProviderKind::OpenAi, &openai, &cancel).unwrap();
        assert!(response.error.is_some());
        assert!(response.images.is_empty());
        assert_eq!(response.usage.input_tokens, Some(42));
        assert_eq!(response.usage.output_tokens, Some(100));
        let gemini = json!({"candidates":[{"finishReason":"MAX_TOKENS"}],"usageMetadata":{"promptTokenCount":30,"candidatesTokenCount":20,"thoughtsTokenCount":10}});
        let response = crate::native::parse_images(ProviderKind::Gemini, &gemini, &cancel).unwrap();
        assert!(response.error.is_some());
        assert!(response.images.is_empty());
        assert_eq!(response.usage.input_tokens, Some(30));
        assert_eq!(response.usage.output_tokens, Some(30));
        assert_eq!(response.usage.reasoning_tokens, Some(10));
        let no_images = json!({"data":[],"usage":{"input_tokens":7,"output_tokens":1}});
        let response = crate::native::parse_images(ProviderKind::OpenAi, &no_images, &cancel).unwrap();
        assert!(response.error.is_some());
        assert_eq!(response.usage.input_tokens, Some(7));
    }

    #[test]
    fn provider_usage_measures_only_reported_fields() {
        let openai = json!({"status":"completed","output":[{"content":[{"type":"output_text","text":"{\"actions\":[]}"}]}],
            "usage":{"input_tokens":100,"output_tokens":30,"input_tokens_details":{"cached_tokens":25},"output_tokens_details":{"reasoning_tokens":10}}});
        assert_eq!(
            parse_generation(ProviderKind::OpenAi, &openai).unwrap().usage,
            Usage {
                input_tokens: Some(100),
                output_tokens: Some(30),
                cached_input_tokens: Some(25),
                cache_write_tokens: None,
                reasoning_tokens: Some(10)
            }
        );
        let gemini = json!({"candidates":[{"finishReason":"STOP","content":{"parts":[{"thought":true,"text":"private reasoning"},{"text":"{}"}]}}],
            "usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"thoughtsTokenCount":3}});
        let response = parse_generation(ProviderKind::Gemini, &gemini).unwrap();
        assert_eq!(response.text, "{}");
        assert_eq!(response.usage.cached_input_tokens, None);
        assert_eq!(response.usage.reasoning_tokens, Some(3));
        assert_eq!(response.usage.output_tokens, Some(5));
        let ollama = json!({"done":true,"message":{"content":"{}"},"eval_count":0});
        assert_eq!(parse_generation(ProviderKind::Ollama, &ollama).unwrap().usage.output_tokens, Some(0));
        assert_eq!(parse_generation(ProviderKind::Ollama, &ollama).unwrap().usage.input_tokens, None);
    }

    #[test]
    fn refusal_and_truncation_preserve_measured_usage() {
        for value in [
            json!({"status":"incomplete","usage":{"input_tokens":7,"output_tokens":5}}),
            json!({"status":"completed","output":[{"content":[{"type":"refusal","refusal":"test-secret-never-echo"}]}],"usage":{"input_tokens":7,"output_tokens":5}}),
        ] {
            let response = parse_generation(ProviderKind::OpenAi, &value).unwrap();
            assert_eq!(response.usage.input_tokens, Some(7));
            assert_eq!(response.usage.output_tokens, Some(5));
            assert!(!response.error.unwrap().contains("test-secret-never-echo"));
        }
        let ollama =
            parse_generation(ProviderKind::Ollama, &json!({"done":true,"done_reason":"length","message":{"content":"{}"},"eval_count":12})).unwrap();
        assert!(ollama.error.is_some());
        assert_eq!(ollama.usage.output_tokens, Some(12));
        let gemini = parse_generation(
            ProviderKind::Gemini,
            &json!({
                "candidates":[{"finishReason":"MAX_TOKENS","content":{"parts":[{"text":"{\"title\":\"partial"}]}}],
                "usageMetadata":{"promptTokenCount":20,"candidatesTokenCount":2,"thoughtsTokenCount":3}
            }),
        )
        .unwrap();
        assert_eq!(gemini.error.as_deref(), Some("Gemini response did not finish normally"));
        assert_eq!(gemini.text, "{\"title\":\"partial");
        assert_eq!(gemini.usage.output_tokens, Some(5));
        let gemini_complete = parse_generation(
            ProviderKind::Gemini,
            &json!({
                "candidates":[{"finishReason":"MAX_TOKENS","content":{"parts":[{"text":"{\"title\":\"complete\",\"commands\":[],\"images\":[]}"}]}}],
                "usageMetadata":{"promptTokenCount":20,"candidatesTokenCount":12,"thoughtsTokenCount":4}
            }),
        )
        .unwrap();
        assert!(gemini_complete.error.is_none());
        assert_eq!(gemini_complete.text, "{\"title\":\"complete\",\"commands\":[],\"images\":[]}");
        assert_eq!(gemini_complete.usage.output_tokens, Some(16));
        let mut model = selection(ProviderKind::Gemini, None);
        model.model = "../key".into();
        assert!(generation_body(&model, &request()).is_err());
    }

    #[test]
    fn gemini_thinking_config_caps_reasoning_tokens_for_json_layout_planning() {
        let mut req = request();
        req.max_output_tokens = 16384;
        for (model, expected) in [
            ("gemini-flash-latest", json!({"thinkingBudget": 1024})),
            ("gemini-2.5-flash", json!({"thinkingBudget": 1024})),
            ("gemini-2.5-flash-lite", json!({"thinkingBudget": 0})),
            ("gemini-2.5-pro", json!({"thinkingBudget": 2048})),
            ("gemini-3.8-flash", json!({"thinkingLevel": "low"})),
        ] {
            let sel = ModelSelection { provider: ProviderKind::Gemini, model: model.into(), endpoint: None };
            let body = generation_body(&sel, &req).unwrap();
            assert_eq!(body.pointer("/generationConfig/thinkingConfig"), Some(&expected), "{model}");
        }
    }

    #[test]
    fn invalid_model_json_remains_available_for_worker_repair() {
        let openai = json!({"status":"completed","output":[{"content":[{"type":"output_text","text":"{broken"}]}],"usage":{"input_tokens":10,"output_tokens":4}});
        let response = parse_generation(ProviderKind::OpenAi, &openai).unwrap();
        assert_eq!(response.text, "{broken");
        assert_eq!(response.usage.output_tokens, Some(4));
        assert!(response.error.is_none());
    }

    #[test]
    fn ollama_mock_checks_http_and_response() {
        let (endpoint, server) = mock(vec![(200, json!({"done":true,"message":{"content":"{}"},"prompt_eval_count":7,"eval_count":2}).to_string())]);
        let response = generate(&selection(ProviderKind::Ollama, Some(&endpoint)), None, &request(), &AtomicBool::new(false)).unwrap();
        assert_eq!(response.usage.input_tokens, Some(7));
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /api/chat HTTP/1.1"));
        let (_, body) = requests[0].split_once("\r\n\r\n").unwrap();
        assert_eq!(serde_json::from_str::<Value>(body).unwrap().get("stream"), Some(&json!(false)));
    }

    #[test]
    fn auth_error_body_never_leaks_secret() {
        let secret = "test-secret-never-echo";
        let (endpoint, server) = mock(vec![(401, format!("{{\"error\":\"{secret}\"}}"))]);
        let error = list_models(&selection(ProviderKind::Ollama, Some(&endpoint)), Some(secret), &AtomicBool::new(false)).unwrap_err();
        assert!(error.contains("401"));
        assert!(!error.contains(secret));
        let requests = server.join().unwrap();
        assert!(requests[0].contains(&format!("Bearer {secret}")));
        assert!(!requests[0].lines().next().unwrap().contains(secret));
    }

    #[test]
    fn malformed_http_json_and_oversized_responses_fail() {
        for body in ["{\"models\":".into(), "x".repeat(MAX_JSON_BYTES + 1)] {
            let (endpoint, server) = mock(vec![(200, body)]);
            assert!(list_models(&selection(ProviderKind::Ollama, Some(&endpoint)), None, &AtomicBool::new(false)).is_err());
            // An oversize Content-Length may close the connection before body delivery.
            let _ = server.join();
        }
    }

    #[test]
    fn comfy_queue_history_view_roundtrip() {
        // A valid PNG signature suffices here; engine image import validates dimensions.
        let image = String::from("RIFFxxxxWEBPtest");
        let (endpoint,server)=mock(vec![(200,json!({"prompt_id":"job-1"}).to_string()),
            (200,json!({"job-1":{"status":{"status_str":"success"},"outputs":{"9":{"images":[{"filename":"image.webp","subfolder":"","type":"output"}]}}}}).to_string()),(200,image)]);
        let images = generate_comfy(
            &selection(ProviderKind::ComfyUi, Some(&endpoint)),
            None,
            &ComfyRequest { workflow: json!({"1":{"class_type":"SaveImage","inputs":{}}}), poll_interval_ms: 50, timeout_seconds: 5 },
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(images[0].mime_type, "image/webp");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /prompt "));
        assert!(requests[1].starts_with("GET /history/job-1 "));
        assert!(requests[2].starts_with("GET /view?filename=image.webp&subfolder=&type=output "));
    }

    #[test]
    fn cancellation_after_submission_deletes_only_own_pending_job() {
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let (endpoint, server) = mock_cancellable(vec![(200, json!({"prompt_id":"own-job"}).to_string()), (200, "{}".into())], Some(cancel.clone()));
        let error = generate_comfy(
            &selection(ProviderKind::ComfyUi, Some(&endpoint)),
            None,
            &ComfyRequest { workflow: json!({"1":{"class_type":"SaveImage","inputs":{}}}), poll_interval_ms: 50, timeout_seconds: 5 },
            &cancel,
        )
        .unwrap_err();
        assert!(error.contains("cancelled"));
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("POST /queue "));
        assert!(requests[1].ends_with("{\"delete\":[\"own-job\"]}"));
        assert!(requests.iter().all(|request| !request.contains("/interrupt")));
    }

    #[test]
    fn comfy_actual_task_prompt_and_model_reach_http_workflow() {
        let roundtrip = || {
            vec![(200,json!({"prompt_id":"job-1"}).to_string()),
            (200,json!({"job-1":{"status":{"status_str":"success"},"outputs":{"9":{"images":[{"filename":"image.webp","subfolder":"","type":"output"}]}}}}).to_string()),
            (200,"RIFFxxxxWEBPtest".into())]
        };
        let responses = roundtrip().into_iter().chain(roundtrip()).collect();
        let (endpoint, server) = mock(responses);
        let mut selected = selection(ProviderKind::ComfyUi, Some(&endpoint));
        selected.model = "chosen-model.safetensors".into();
        let workflow = json!({"user-defined-node":{"class_type":"WhateverInstalledNode","inputs":{"nested":[{"text":"Positive: {{prompt}}","checkpoint":"{{model}}"}]}}});
        for prompt in ["مقهى {{model}}", "مكتبة ثانية"] {
            let request = ImageRequest {
                prompt: prompt.into(),
                size: None,
                quality: None,
                comfy: Some(ComfyRequest { workflow: workflow.clone(), poll_interval_ms: 50, timeout_seconds: 5 }),
                resources: Default::default(),
            };
            assert_eq!(generate_images(&selected, None, &request, &AtomicBool::new(false)).unwrap().images.len(), 1);
        }
        let requests = server.join().unwrap();
        let body = |index: usize| serde_json::from_str::<Value>(requests[index].split_once("\r\n\r\n").unwrap().1).unwrap();
        let first = body(0);
        let second = body(3);
        assert_ne!(first, second);
        assert_eq!(first.pointer("/prompt/user-defined-node/inputs/nested/0/text"), Some(&json!("Positive: مقهى {{model}}")));
        assert_eq!(second.pointer("/prompt/user-defined-node/inputs/nested/0/text"), Some(&json!("Positive: مكتبة ثانية")));
        assert_eq!(first.pointer("/prompt/user-defined-node/inputs/nested/0/checkpoint"), Some(&json!("chosen-model.safetensors")));
        assert_eq!(second.pointer("/prompt/user-defined-node/inputs/nested/0/checkpoint"), Some(&json!("chosen-model.safetensors")));
    }

    #[test]
    fn comfy_requires_explicit_bounded_workflow_bindings() {
        use crate::native::bind_comfy_workflow;
        assert!(bind_comfy_workflow(&json!({"text":"fixed","checkpoint":"{{model}}"}), "task", "model").is_err());
        assert!(bind_comfy_workflow(&json!({"text":"{{prompt}}","checkpoint":"fixed"}), "task", "model").is_err());
        assert!(bind_comfy_workflow(&json!({"{{prompt}}":"fixed","checkpoint":"{{model}}"}), "task", "model").is_err());
        let prompt = "x".repeat(MAX_JSON_BYTES / 2);
        assert!(bind_comfy_workflow(&json!({"text":"{{prompt}}{{prompt}}{{prompt}}"}), &prompt, "").is_err());
        assert_eq!(bind_comfy_workflow(&json!({"text":"{{prompt}}"}), "task", "").unwrap(), json!({"text":"task"}));
    }
}
