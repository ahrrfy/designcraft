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

    // SEC-07: Cloud providers must reject private, loopback, link-local, metadata, CGNAT, unique-local, and internal hosts even over HTTPS.
    for cloud_provider in [ProviderKind::OpenAi, ProviderKind::Gemini] {
        for ssrf_endpoint in [
            "https://127.0.0.1:8443/v1",
            "https://127.99.1.2/v1",
            "https://[::1]/v1",
            "https://0.0.0.0/v1",
            "https://[::]/v1",
            "https://10.0.0.1/v1",
            "https://172.16.0.1/v1",
            "https://172.31.255.254/v1",
            "https://192.168.1.1/v1",
            "https://169.254.169.254/latest/meta-data",
            "https://[fe80::1]/v1",
            "https://100.64.0.1/v1",
            "https://100.127.255.254/v1",
            "https://[fc00::1]/v1",
            "https://[fd12:3456:789a::1]/v1",
            "https://[::ffff:127.0.0.1]/v1",
            "https://[::ffff:169.254.169.254]/v1",
            "https://2130706433/v1",
            "https://localhost/v1",
            "https://sub.localhost/v1",
            "https://printer.local/v1",
            "https://metadata.google.internal/v1",
            "https://user:pass@api.openai.com/v1",
        ] {
            assert!(
                validate_endpoint(&selection(cloud_provider, Some(ssrf_endpoint))).is_err(),
                "expected {cloud_provider:?} to reject SSRF endpoint {ssrf_endpoint}"
            );
        }
        for public_endpoint in ["https://proxy.example.com/v1", "https://172.15.0.1/v1", "https://172.32.0.1/v1", "https://100.63.255.254/v1"] {
            assert!(
                validate_endpoint(&selection(cloud_provider, Some(public_endpoint))).is_ok(),
                "expected {cloud_provider:?} to accept public HTTPS endpoint {public_endpoint}"
            );
        }
    }

    // Local providers allow only localhost, 127.0.0.1, or [::1], and reject metadata IPs and URL credentials.
    for local_provider in [ProviderKind::Ollama, ProviderKind::ComfyUi] {
        for allowed in ["http://localhost:11434", "https://localhost:11434", "http://127.0.0.1:8188", "http://[::1]:8188"] {
            assert!(validate_endpoint(&selection(local_provider, Some(allowed))).is_ok(), "{allowed}");
        }
        for rejected in [
            "http://169.254.169.254/",
            "https://169.254.169.254/",
            "http://user:pass@127.0.0.1:11434/",
            "https://user:pass@localhost:11434/",
            "http://127.0.0.2:11434/",
            "https://10.0.0.5:11434/",
            "https://metadata.google.internal/",
        ] {
            assert!(validate_endpoint(&selection(local_provider, Some(rejected))).is_err(), "{rejected}");
        }
    }
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
        assert_eq!(gemini.error.as_deref(), Some("Gemini response did not finish normally (MAX_TOKENS)"));
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

    #[test]
    fn ai28_gemini_response_json_schema_validator_and_image_modalities() {
        let closed_schema = json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["title", "pages"],
            "properties": {
                "title": {"type": "string"},
                "pages": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 2,
                    "items": {
                        "anyOf": [{
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["page", "illustrations"],
                            "properties": {
                                "page": {"type": "integer"},
                                "illustrations": {
                                    "type": "array",
                                    "maxItems": 3,
                                    "items": {
                                        "type": "object",
                                        "properties": {"slot": {"type": "string"}, "prompt": {"type": "string"}}
                                    }
                                }
                            }
                        }]
                    }
                }
            }
        });
        assert!(crate::native::gemini_supports_json_schema(&closed_schema));
        let mut req = request();
        req.json_schema = closed_schema.clone();
        for model in ["gemini-2.5-flash", "gemini-3.8-flash", "gemini-4-argon"] {
            let sel = ModelSelection { provider: ProviderKind::Gemini, model: model.into(), endpoint: None };
            let body = generation_body(&sel, &req).unwrap();
            assert_eq!(body.pointer("/generationConfig/responseJsonSchema"), Some(&closed_schema), "{model}");
            assert!(body.pointer("/contents/0/parts/0/text").and_then(Value::as_str).unwrap().contains("OUTPUT JSON CONTRACT:\n"));
        }
        // Reject empty {}, untyped nodes, invalid type arrays, additionalProperties: true, open objects, maxItems: 0,
        // unsupported combinators (oneOf, allOf, not, const), invalid prefixItems, and depth > 32.
        assert!(crate::native::gemini_supports_json_schema(&json!({
            "anyOf": [{"type": "string"}, {"type": "integer"}]
        })));
        assert!(crate::native::gemini_supports_json_schema(&json!({
            "type": "array",
            "prefixItems": [{"type": "number"}, {"type": "number"}]
        })));
        assert!(!crate::native::gemini_supports_json_schema(&json!({
            "oneOf": [{"type": "string"}, {"type": "integer"}]
        })));
        assert!(!crate::native::gemini_supports_json_schema(&json!({
            "allOf": [{"type": "string"}]
        })));
        assert!(!crate::native::gemini_supports_json_schema(&json!({
            "not": {"type": "null"}
        })));
        assert!(!crate::native::gemini_supports_json_schema(&json!({"const": "fixed"})));
        assert!(!crate::native::gemini_supports_json_schema(&json!({
            "type": "array",
            "prefixItems": []
        })));
        assert!(!crate::native::gemini_supports_json_schema(&json!({
            "type": "array",
            "prefixItems": [{}]
        })));
        assert!(!crate::native::gemini_supports_json_schema(&json!({})));
        assert!(!crate::native::gemini_supports_json_schema(&json!({"description": "any"})));
        assert!(!crate::native::gemini_supports_json_schema(&json!({"type": []})));
        assert!(!crate::native::gemini_supports_json_schema(&json!({
            "type": "object",
            "properties": {"a": {"type": "string"}},
            "additionalProperties": true
        })));
        assert!(!crate::native::gemini_supports_json_schema(&json!({"type": "object"})));
        assert!(!crate::native::gemini_supports_json_schema(&json!({"type": "object", "properties": {}})));
        assert!(!crate::native::gemini_supports_json_schema(&json!({
            "type": "object",
            "properties": {"empty": {"type": "array", "maxItems": 0, "items": {"type": "string"}}}
        })));
        let mut deep = json!({"type": "string"});
        for _ in 0..34 {
            deep = json!({"type": "object", "properties": {"nested": deep}});
        }
        assert!(!crate::native::gemini_supports_json_schema(&deep));

        // Mixed-case Gemini print tiers ("Gemini-3.1-Flash-Image" @2K) and ratio validation without @tier; OpenAI DALL-E b64_json.
        assert_eq!(gemini_print_size("Gemini-3.1-Flash-Image", "1:4", [900, 3600]).unwrap(), "1:4@2K");
        assert_eq!(gemini_print_size("MODELS/Gemini-3.1-Flash-Image ", "1:4", [900, 3600]).unwrap(), "1:4@2K");
        let mixed_tier = crate::native::image_generation_body(
            ProviderKind::Gemini,
            "Gemini-3.1-Flash-Image",
            &ImageRequest {
                prompt: "Clean studio icon".into(),
                size: Some("1:4@2K".into()),
                quality: None,
                comfy: None,
                resources: Default::default(),
            },
        )
        .unwrap();
        assert_eq!(mixed_tier["generationConfig"]["imageConfig"]["aspectRatio"], "1:4");
        assert_eq!(mixed_tier["generationConfig"]["imageConfig"]["imageSize"], "2K");
        let img_req =
            ImageRequest { prompt: "Clean studio icon".into(), size: Some("4:1".into()), quality: None, comfy: None, resources: Default::default() };
        let gemini_lite = crate::native::image_generation_body(ProviderKind::Gemini, "gemini-3.1-flash-lite-image", &img_req).unwrap();
        assert_eq!(gemini_lite["generationConfig"]["responseModalities"], json!(["IMAGE"]));
        assert_eq!(gemini_lite["generationConfig"]["imageConfig"]["aspectRatio"], "4:1");
        assert!(crate::native::image_generation_body(ProviderKind::Gemini, "gemini-2.5-flash-image", &img_req).is_err());

        let dalle = crate::native::image_generation_body(
            ProviderKind::OpenAi,
            "dall-e-3",
            &ImageRequest { size: Some("1792x1024".into()), ..img_req.clone() },
        )
        .unwrap();
        assert_eq!(dalle["response_format"], "b64_json");
        assert_eq!(dalle["size"], "1792x1024");
        let gpt_img =
            crate::native::image_generation_body(ProviderKind::OpenAi, "gpt-image-1", &ImageRequest { size: Some("1536x1024".into()), ..img_req })
                .unwrap();
        assert!(gpt_img.get("response_format").is_none());
    }

    #[test]
    fn ai28_distinct_http_diagnostics_sanitized_reasons_and_bounded_429_503_retries() {
        let cancel = AtomicBool::new(false);
        for (status, expected) in [
            (400, "AI provider HTTP 400: invalid request or unsupported model parameter"),
            (404, "AI provider HTTP 404: model or endpoint not found"),
            (422, "AI provider HTTP 422: request schema or parameters rejected"),
            (502, "AI provider HTTP 502: provider temporarily unavailable"),
        ] {
            let (endpoint, server) = mock(vec![(status, "{\"error\":\"secret-detail\"}".into())]);
            let err = list_models(&selection(ProviderKind::Ollama, Some(&endpoint)), None, &cancel).unwrap_err();
            assert_eq!(err, expected);
            assert_eq!(server.join().unwrap().len(), 1);
        }

        // Sanitized finishReason / blockReason tokens vs hostile payloads.
        let blocked = parse_generation(ProviderKind::Gemini, &json!({"promptFeedback": {"blockReason": "PROHIBITED_CONTENT"}})).unwrap();
        assert_eq!(blocked.error.as_deref(), Some("Gemini returned no candidate (PROHIBITED_CONTENT)"));
        let hostile_reason =
            parse_generation(ProviderKind::Gemini, &json!({"candidates": [{"finishReason": "secret/token leak", "content": {"parts": []}}]}))
                .unwrap();
        assert_eq!(hostile_reason.error.as_deref(), Some("Gemini response did not finish normally"));
        let img_safety =
            crate::native::parse_images(ProviderKind::Gemini, &json!({"candidates": [{"finishReason": "IMAGE_SAFETY"}]}), &cancel).unwrap();
        assert_eq!(img_safety.error.as_deref(), Some("Gemini image response did not finish normally (IMAGE_SAFETY)"));

        // Bounded HTTP 429 + 503 retry succeeds on attempt 3.
        let ok_body = json!({"done":true,"message":{"content":"{}"},"prompt_eval_count":5,"eval_count":2}).to_string();
        let (endpoint, server) = mock(vec![(429, "{}".into()), (503, "{}".into()), (200, ok_body)]);
        let recovered = generate(&selection(ProviderKind::Ollama, Some(&endpoint)), None, &request(), &cancel).unwrap();
        assert_eq!(recovered.text, "{}");
        assert_eq!(server.join().unwrap().len(), 3);

        // Exhausting 2 retries (3 total attempts) surfaces distinct 503 diagnostic.
        let (endpoint, server) = mock(vec![(503, "{}".into()), (503, "{}".into()), (503, "{}".into())]);
        let exhausted = generate(&selection(ProviderKind::Ollama, Some(&endpoint)), None, &request(), &cancel).unwrap_err();
        assert_eq!(exhausted, "AI provider HTTP 503: provider temporarily unavailable");
        assert_eq!(server.join().unwrap().len(), 3);

        // Cancellation aborts immediately without waiting for retry attempts.
        let cancel_mid = std::sync::Arc::new(AtomicBool::new(false));
        let (endpoint, server) = mock_cancellable(vec![(429, "{}".into())], Some(cancel_mid.clone()));
        let cancelled = generate(&selection(ProviderKind::Ollama, Some(&endpoint)), None, &request(), &cancel_mid).unwrap_err();
        assert_eq!(cancelled, "AI request cancelled");
        assert_eq!(server.join().unwrap().len(), 1);

        // ComfyUI prompt submission uses single-shot post_once: a 503 returns immediately (1 request, no un-cancellable retry backoff).
        let (endpoint, server) = mock(vec![(503, "{}".into())]);
        let comfy_sel = selection(ProviderKind::ComfyUi, Some(&endpoint));
        let comfy_req =
            ComfyRequest { workflow: json!({"1": {"class_type": "Node", "inputs": {"text": "ok"}}}), poll_interval_ms: 50, timeout_seconds: 5 };
        let start = std::time::Instant::now();
        let comfy_err = generate_comfy(&comfy_sel, None, &comfy_req, &cancel).unwrap_err();
        assert_eq!(comfy_err, "AI provider HTTP 503: provider temporarily unavailable");
        assert!(start.elapsed() < std::time::Duration::from_millis(200));
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn ai29_gemini_schema_prep_strips_string_lengths_omits_explosive_unions_and_recovers_config_400() {
        // 1. Compact closed schema with unsupported string constraints strips minLength/maxLength/pattern
        //    from responseJsonSchema while keeping the full contract in the user prompt.
        let compact_with_lengths = GenerationRequest {
            json_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["title", "summary"],
                "properties": {
                    "title": {"type": "string", "minLength": 1, "maxLength": 120},
                    "summary": {"type": "string", "minLength": 1, "maxLength": 400, "pattern": "^.+$"}
                }
            }),
            ..request()
        };
        let sel = ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.7-flash".into(), endpoint: None };
        let compact_body = generation_body(&sel, &compact_with_lengths).unwrap();
        let attached = &compact_body["generationConfig"]["responseJsonSchema"];
        assert!(attached.is_object(), "compact sanitized schema should stay attached");
        assert!(attached["properties"]["title"].get("minLength").is_none());
        assert!(attached["properties"]["title"].get("maxLength").is_none());
        assert!(attached["properties"]["summary"].get("pattern").is_none());
        assert!(compact_body["contents"][0]["parts"][0]["text"].as_str().unwrap().contains("\"maxLength\":120"));

        // 2. Multi-page 9-family union schema (Batch 2 facing spread: minItems=2, maxItems=2)
        //    omits responseJsonSchema so Gemini's grammar compiler never rejects Page 2+ with HTTP 400.
        let branches: Vec<Value> = (0..9)
            .map(|idx| {
                let mut props = serde_json::Map::new();
                let mut req_slots = Vec::new();
                for s in 0..12 {
                    let key = format!("slot_{s}");
                    props.insert(key.clone(), json!({"type": "string", "minLength": 1, "maxLength": 96}));
                    req_slots.push(key);
                }
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["page", "family", "text"],
                    "properties": {
                        "page": {"type": "integer", "minimum": 1, "maximum": 2},
                        "family": {"type": "string", "enum": [format!("family-{idx}")]},
                        "text": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": req_slots,
                            "properties": props
                        }
                    }
                })
            })
            .collect();
        let multipage_req = GenerationRequest {
            json_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["title", "pages"],
                "properties": {
                    "title": {"type": "string", "minLength": 1, "maxLength": 120},
                    "pages": {
                        "type": "array",
                        "minItems": 2,
                        "maxItems": 2,
                        "items": {"anyOf": branches}
                    }
                }
            }),
            ..request()
        };
        let multipage_body = generation_body(&sel, &multipage_req).unwrap();
        assert_eq!(multipage_body["generationConfig"]["responseMimeType"], "application/json");
        assert!(
            multipage_body["generationConfig"].get("responseJsonSchema").is_none(),
            "explosive 9-branch multi-page union must omit responseJsonSchema and rely on JSON mode + prompt contract"
        );
        assert!(multipage_body["contents"][0]["parts"][0]["text"].as_str().unwrap().contains("OUTPUT JSON CONTRACT:"));

        // 3. Pre-inference HTTP 400 fallback strips optional thinkingConfig/responseJsonSchema once for Gemini.
        let cancel = AtomicBool::new(false);
        let mut calls: Vec<(Value, bool)> = Vec::new();
        let ok_payload = json!({
            "candidates": [{"finishReason": "STOP", "content": {"parts": [{"text": "{\"title\":\"ok\",\"summary\":\"ok\"}"}]}}],
            "usageMetadata": {"promptTokenCount": 42, "candidatesTokenCount": 12}
        });
        let recovered = crate::native::generate_with_post(&sel, &compact_with_lengths, &cancel, |_, body, once| {
            calls.push((body.clone(), once));
            if calls.len() == 1 { Err("AI provider HTTP 400: invalid request or unsupported model parameter".into()) } else { Ok(ok_payload.clone()) }
        })
        .unwrap();
        assert_eq!(recovered.text, "{\"title\":\"ok\",\"summary\":\"ok\"}");
        assert_eq!(calls.len(), 2);
        assert!(!calls[0].1);
        assert!(calls[0].0["generationConfig"].get("thinkingConfig").is_some());
        assert!(calls[0].0["generationConfig"].get("responseJsonSchema").is_some());
        assert!(calls[1].1, "fallback must be single-shot");
        assert!(calls[1].0["generationConfig"].get("thinkingConfig").is_none());
        assert!(calls[1].0["generationConfig"].get("responseJsonSchema").is_none());
        assert_eq!(calls[1].0["generationConfig"]["responseMimeType"], "application/json");

        // Persistent HTTP 400 after stripping optional config returns sanitized error without further retries.
        let mut fail_calls = 0usize;
        let still_bad = crate::native::generate_with_post(&sel, &compact_with_lengths, &cancel, |_, _, _| {
            fail_calls += 1;
            Err("AI provider HTTP 400: invalid request or unsupported model parameter".into())
        })
        .unwrap_err();
        assert_eq!(still_bad, "AI provider HTTP 400: invalid request or unsupported model parameter");
        assert_eq!(fail_calls, 2);
    }

    #[test]
    fn bug02_stalled_blocking_http_call_cancels_under_400ms() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        let release_stall = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let server_release = std::sync::Arc::clone(&release_stall);
        let server = std::thread::spawn(move || {
            if let Ok((mut socket, _)) = listener.accept() {
                let _ = socket.set_read_timeout(Some(std::time::Duration::from_millis(200)));
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf);
                // Stall without replying for up to 10 seconds until the test releases cleanup.
                let (lock, cvar) = &*server_release;
                let guard = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let _ = cvar.wait_timeout_while(guard, std::time::Duration::from_secs(10), |released| !*released);
            }
        });

        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let cancel_trigger = std::sync::Arc::clone(&cancel);
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(80));
            cancel_trigger.store(true, Ordering::Relaxed);
        });

        let sel = selection(ProviderKind::Ollama, Some(&endpoint));
        let started = std::time::Instant::now();
        let err = generate(&sel, None, &request(), &cancel).unwrap_err();
        let elapsed = started.elapsed();

        canceller.join().unwrap();
        assert_eq!(err, "AI request cancelled");
        assert!(elapsed < std::time::Duration::from_millis(400), "expected cancellation in < 400ms, took {elapsed:?}");

        // While the cancelled blocking request is still stalled on the local server, a second
        // local generation to that origin must be rejected rather than bypassing single-request admission.
        let concurrent_err = generate(&sel, None, &request(), &AtomicBool::new(false)).unwrap_err();
        assert!(concurrent_err.contains("Another local inference is running"), "{concurrent_err}");

        let (lock, cvar) = &*release_stall;
        *lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        cvar.notify_all();
        server.join().unwrap();

        // Once the detached HTTP worker finishes unwinding, the local origin slot is released.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let probe = generate(&sel, None, &request(), &AtomicBool::new(false));
            if probe.as_ref().is_err_and(|msg| !msg.contains("Another local inference is running")) {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "local origin slot remained stuck after worker exit: {probe:?}");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn gemini_agent_turn_preserves_thought_signatures_and_cleans_tool_schemas() {
        let cancel = AtomicBool::new(false);
        let sel = ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.8-flash".into(), endpoint: None };
        let tools = vec![ToolDefinition {
            name: "inspect_pages".into(),
            description: "Inspect document pages".into(),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "start": {"type": "integer", "minimum": 1},
                    "end": {"type": "integer", "minimum": 1},
                    "filter": {"type": "string", "minLength": 1, "pattern": "^[a-z]+$"}
                },
                "required": ["start", "end"]
            }),
        }];
        let req_turn1 = AgentTurnRequest {
            system: "Inspect before editing".into(),
            messages: vec![AgentMessage::User { text: "Fix Arabic fonts on all pages".into(), images: Vec::new() }],
            tools: tools.clone(),
            max_output_tokens: 4096,
            resources: Default::default(),
        };
        let mut seen_bodies = Vec::new();
        let turn1 = crate::agent::generate_agent_turn_with_post(&sel, &req_turn1, &cancel, |path, body, _| {
            assert_eq!(path, "models/gemini-3.8-flash:generateContent");
            seen_bodies.push(body.clone());
            Ok(json!({
                "candidates": [{
                    "finishReason": "STOP",
                    "content": {
                        "role": "model",
                        "parts": [
                            {"thought": true, "text": "Need to inspect pages 1..32 first", "thoughtSignature": "sig-thought-1"},
                            {"functionCall": {"name": "inspect_pages", "args": {"start": 1, "end": 32}}, "thoughtSignature": "sig-call-1"}
                        ]
                    }
                }],
                "usageMetadata": {"promptTokenCount": 120, "candidatesTokenCount": 18, "thoughtsTokenCount": 42}
            }))
        })
        .unwrap();
        assert!(turn1.error.is_none());
        assert_eq!(turn1.usage.input_tokens, Some(120));
        assert_eq!(turn1.usage.output_tokens, Some(60));
        assert_eq!(turn1.tool_calls.len(), 1);
        assert_eq!(turn1.tool_calls[0].name, "inspect_pages");
        assert_eq!(turn1.tool_calls[0].arguments, json!({"start": 1, "end": 32}));
        // Schema unsupported keywords were stripped for Gemini functionDeclarations:
        let decl_params = &seen_bodies[0]["tools"][0]["functionDeclarations"][0]["parameters"];
        assert!(decl_params.get("additionalProperties").is_none());
        assert!(decl_params["properties"]["filter"].get("minLength").is_none());
        // responseMimeType must NOT be set alongside tools:
        assert!(seen_bodies[0]["generationConfig"].get("responseMimeType").is_none());

        // Turn 2 replays raw_provider_turn so Gemini 3 thoughtSignature is preserved verbatim!
        let req_turn2 = AgentTurnRequest {
            system: "Inspect before editing".into(),
            messages: vec![
                AgentMessage::User { text: "Fix Arabic fonts on all pages".into(), images: Vec::new() },
                AgentMessage::Assistant {
                    text: turn1.text.clone(),
                    tool_calls: turn1.tool_calls.clone(),
                    raw_provider_turn: turn1.raw_provider_turn.clone(),
                },
                AgentMessage::ToolResults {
                    results: vec![ToolResultMessage {
                        call_id: turn1.tool_calls[0].id.clone(),
                        name: "inspect_pages".into(),
                        output: json!({"pageCount": 32}),
                        images: Vec::new(),
                    }],
                },
            ],
            tools,
            max_output_tokens: 4096,
            resources: Default::default(),
        };
        let turn2 = crate::agent::generate_agent_turn_with_post(&sel, &req_turn2, &cancel, |_, body, _| {
            assert_eq!(body["contents"][1]["parts"][1]["thoughtSignature"], "sig-call-1");
            assert_eq!(body["contents"][2]["parts"][0]["functionResponse"]["name"], "inspect_pages");
            assert_eq!(body["contents"][2]["parts"][0]["functionResponse"]["response"]["pageCount"], 32);
            Ok(json!({
                "candidates": [{
                    "finishReason": "STOP",
                    "content": {"role": "model", "parts": [{"text": "Completed font replacement across all 32 pages."}]}
                }],
                "usageMetadata": {"promptTokenCount": 300, "candidatesTokenCount": 14}
            }))
        })
        .unwrap();
        assert!(turn2.error.is_none());
        assert!(turn2.tool_calls.is_empty());
        assert_eq!(turn2.text, "Completed font replacement across all 32 pages.");
    }

    #[test]
    fn openai_agent_turn_replays_reasoning_and_function_calls() {
        let cancel = AtomicBool::new(false);
        let sel = ModelSelection { provider: ProviderKind::OpenAi, model: "gpt-5.4".into(), endpoint: None };
        let tools = vec![ToolDefinition {
            name: "apply_engine_commands".into(),
            description: "Execute engine commands".into(),
            parameters: json!({"type": "object", "properties": {"commands": {"type": "array", "items": {"type": "object"}}}}),
        }];
        let req1 = AgentTurnRequest {
            system: "Use native commands".into(),
            messages: vec![AgentMessage::User { text: "Replace Amiri with Sakkal Majalla".into(), images: Vec::new() }],
            tools: tools.clone(),
            max_output_tokens: 2048,
            resources: Default::default(),
        };
        let turn1 = crate::agent::generate_agent_turn_with_post(&sel, &req1, &cancel, |path, body, _| {
            assert_eq!(path, "responses");
            assert_eq!(body["tools"][0]["name"], "apply_engine_commands");
            assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
            Ok(json!({
                "status": "completed",
                "output": [
                    {"type": "reasoning", "id": "rs_unencrypted", "summary": []},
                    {"type": "reasoning", "id": "rs_enc", "summary": [], "encrypted_content": "enc_blob_123"},
                    {"type": "function_call", "id": "fc_1", "status": "completed", "call_id": "call_abc", "name": "apply_engine_commands", "arguments": "{\"commands\":[{\"command\":\"font.replace\"}]}"}
                ],
                "usage": {"input_tokens": 90, "output_tokens": 25}
            }))
        })
        .unwrap();
        assert_eq!(turn1.tool_calls.len(), 1);
        assert_eq!(turn1.tool_calls[0].id, "call_abc");
        assert_eq!(turn1.tool_calls[0].arguments["commands"][0]["command"], "font.replace");

        let req2 = AgentTurnRequest {
            system: "Use native commands".into(),
            messages: vec![
                AgentMessage::User { text: "Replace Amiri with Sakkal Majalla".into(), images: Vec::new() },
                AgentMessage::Assistant { text: turn1.text, tool_calls: turn1.tool_calls, raw_provider_turn: turn1.raw_provider_turn },
                AgentMessage::ToolResults {
                    results: vec![ToolResultMessage {
                        call_id: "call_abc".into(),
                        name: "apply_engine_commands".into(),
                        output: json!({"ok": true, "replaced": 48}),
                        images: Vec::new(),
                    }],
                },
            ],
            tools,
            max_output_tokens: 2048,
            resources: Default::default(),
        };
        let turn2 = crate::agent::generate_agent_turn_with_post(&sel, &req2, &cancel, |_, body, _| {
            // Unencrypted reasoning item `rs_unencrypted` must be stripped under store=false, while encrypted reasoning is kept!
            assert_eq!(body["input"][1]["type"], "reasoning");
            assert_eq!(body["input"][1]["encrypted_content"], "enc_blob_123");
            assert!(body["input"][1].get("id").is_none());
            assert_eq!(body["input"][2]["type"], "function_call");
            assert!(body["input"][2].get("id").is_none() && body["input"][2].get("status").is_none());
            assert_eq!(body["input"][3]["type"], "function_call_output");
            assert_eq!(body["input"][3]["call_id"], "call_abc");
            Ok(json!({
                "status": "completed",
                "output": [{"type": "message", "content": [{"type": "output_text", "text": "Replaced 48 runs."}]}],
                "usage": {"input_tokens": 130, "output_tokens": 10}
            }))
        })
        .unwrap();
        assert!(turn2.error.is_none());
        assert_eq!(turn2.text, "Replaced 48 runs.");
    }

    #[test]
    fn ollama_agent_turn_supports_native_tools_and_automatic_json_fallback_over_socket() {
        let tool = ToolDefinition {
            name: "inspect_document_overview".into(),
            description: "Inspect open document".into(),
            parameters: json!({"type": "object", "properties": {}}),
        };
        // Turn 1: local Ollama model rejects `tools` with HTTP 400 and falls back to JSON envelope mode.
        // Turn 2: sticky fallback goes straight to JSON envelope mode in 1 HTTP call (no second HTTP 400!).
        let (endpoint, handle) = mock(vec![
            (400, r#"{"error":"model does not support tools"}"#.into()),
            (
                200,
                r#"{"done":true,"prompt_eval_count":64,"eval_count":22,"message":{"role":"assistant","content":"{\"tool_calls\":[{\"name\":\"inspect_document_overview\",\"arguments\":{}}]}"}}"#
                    .into(),
            ),
            (
                200,
                r#"{"done":true,"prompt_eval_count":88,"eval_count":14,"message":{"role":"assistant","content":"{\"final_response\":\"All 32 pages inspected.\"}"}}"#
                    .into(),
            ),
        ]);
        let sel = ModelSelection { provider: ProviderKind::Ollama, model: "qwen2.5:7b".into(), endpoint: Some(endpoint) };
        let req1 = AgentTurnRequest {
            system: "Inspect document".into(),
            messages: vec![AgentMessage::User { text: "Check pages".into(), images: Vec::new() }],
            tools: vec![tool.clone()],
            max_output_tokens: 512,
            resources: Default::default(),
        };
        let resp1 = generate_agent_turn(&sel, None, &req1, &AtomicBool::new(false)).unwrap();
        assert!(resp1.error.is_none());
        assert_eq!(resp1.tool_calls.len(), 1);
        assert_eq!(resp1.tool_calls[0].name, "inspect_document_overview");

        let req2 = AgentTurnRequest {
            system: "Inspect document".into(),
            messages: vec![
                AgentMessage::User { text: "Check pages".into(), images: Vec::new() },
                AgentMessage::Assistant { text: resp1.text, tool_calls: resp1.tool_calls, raw_provider_turn: resp1.raw_provider_turn },
                AgentMessage::ToolResults {
                    results: vec![ToolResultMessage {
                        call_id: "ollama_json_call_0".into(),
                        name: "inspect_document_overview".into(),
                        output: json!({"ok": true, "pageCount": 32}),
                        images: Vec::new(),
                    }],
                },
            ],
            tools: vec![tool],
            max_output_tokens: 512,
            resources: Default::default(),
        };
        let resp2 = generate_agent_turn(&sel, None, &req2, &AtomicBool::new(false)).unwrap();
        let captured = handle.join().unwrap();
        assert_eq!(captured.len(), 3, "Turn 2 must skip native tools and succeed in 1 request");
        assert!(captured[0].contains("\"tools\""));
        assert!(captured[1].contains("\"format\":\"json\"") && !captured[1].contains("\"tools\":[{\"type\":\"function\""));
        assert!(captured[2].contains("\"format\":\"json\"") && !captured[2].contains("\"role\":\"tool\""));
        assert!(resp2.error.is_none());
        assert_eq!(resp2.text, "All 32 pages inspected.");
    }

    #[test]
    fn multimodal_tool_results_serialize_for_vision_models_and_degrade_for_text_only_models() {
        let req = AgentTurnRequest {
            system: "Visually verify layout".into(),
            messages: vec![
                AgentMessage::User { text: "Verify page 1".into(), images: Vec::new() },
                AgentMessage::Assistant {
                    text: String::new(),
                    tool_calls: vec![
                        ToolCall { id: "call_vis_1".into(), name: "render_visual_check".into(), arguments: json!({"page": 1}) },
                        ToolCall { id: "call_pages_2".into(), name: "inspect_pages".into(), arguments: json!({"pages": [1]}) },
                    ],
                    raw_provider_turn: None,
                },
                AgentMessage::ToolResults {
                    results: vec![
                        ToolResultMessage {
                            call_id: "call_vis_1".into(),
                            name: "render_visual_check".into(),
                            output: json!({"ok": true, "page": 1, "width": 680, "height": 960}),
                            images: vec![ImageInput { mime_type: "image/png".into(), bytes: vec![1, 2, 3] }],
                        },
                        ToolResultMessage {
                            call_id: "call_pages_2".into(),
                            name: "inspect_pages".into(),
                            output: json!({"ok": true, "pages": [{"page": 1}]}),
                            images: Vec::new(),
                        },
                    ],
                },
            ],
            tools: vec![
                ToolDefinition {
                    name: "render_visual_check".into(),
                    description: "Render page".into(),
                    parameters: json!({"type": "object", "properties": {"page": {"type": "integer"}}}),
                },
                ToolDefinition {
                    name: "inspect_pages".into(),
                    description: "Inspect pages".into(),
                    parameters: json!({"type": "object", "properties": {"pages": {"type": "array", "items": {"type": "integer"}}}}),
                },
            ],
            max_output_tokens: 512,
            resources: Default::default(),
        };

        // Gemini 3 nests inlineData inside functionResponse.parts and keeps parallel functionResponse entries contiguous:
        let gemini3_sel = ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.8-flash".into(), endpoint: None };
        assert!(supports_agent_vision(&gemini3_sel));
        let gemini3_body = crate::agent::agent_turn_body(&gemini3_sel, &req, false).unwrap();
        let g3_parts = gemini3_body["contents"][2]["parts"].as_array().unwrap();
        assert_eq!(g3_parts.len(), 2, "Gemini 3 must emit 2 contiguous functionResponse parts with media nested inside functionResponse.parts");
        assert_eq!(g3_parts[0]["functionResponse"]["name"], "render_visual_check");
        assert_eq!(g3_parts[0]["functionResponse"]["parts"][0]["inlineData"]["mimeType"], "image/png");
        assert_eq!(g3_parts[0]["functionResponse"]["parts"][0]["inlineData"]["data"], "AQID");
        assert_eq!(g3_parts[1]["functionResponse"]["name"], "inspect_pages");
        assert!(g3_parts[1]["functionResponse"].get("parts").is_none());

        // Gemini 3 HTTP 400 fallback demotes nested functionResponse.parts into trailing sibling inlineData parts:
        let mut g3_posts = Vec::new();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let fallback_resp = crate::agent::generate_agent_turn_with_post(&gemini3_sel, &req, &cancel, |_path, body, once| {
            g3_posts.push((once, body.clone()));
            if !once {
                Err("AI provider HTTP 400: Unknown name \"parts\" at 'contents[2].parts[0].function_response'".into())
            } else {
                Ok(json!({
                    "candidates": [{"content": {"role": "model", "parts": [{"text": "Verified visually"}]}, "finishReason": "STOP"}],
                    "usageMetadata": {"promptTokenCount": 20, "candidatesTokenCount": 5}
                }))
            }
        })
        .unwrap();
        assert_eq!(fallback_resp.text, "Verified visually");
        assert_eq!(g3_posts.len(), 2);
        let demoted_parts = g3_posts[1].1["contents"][2]["parts"].as_array().unwrap();
        assert_eq!(demoted_parts.len(), 3);
        assert!(demoted_parts[0]["functionResponse"].get("parts").is_none());
        assert_eq!(demoted_parts[0]["functionResponse"]["name"], "render_visual_check");
        assert_eq!(demoted_parts[1]["functionResponse"]["name"], "inspect_pages");
        assert_eq!(demoted_parts[2]["inlineData"]["data"], "AQID");

        // Gemini 2.5 emits all functionResponse parts contiguously first, then trailing inlineData parts:
        let gemini25_sel = ModelSelection { provider: ProviderKind::Gemini, model: "gemini-2.5-flash".into(), endpoint: None };
        assert!(supports_agent_vision(&gemini25_sel));
        let gemini25_body = crate::agent::agent_turn_body(&gemini25_sel, &req, false).unwrap();
        let g25_parts = gemini25_body["contents"][2]["parts"].as_array().unwrap();
        assert_eq!(g25_parts.len(), 3, "Gemini 2.5 must keep functionResponses contiguous before inlineData");
        assert_eq!(g25_parts[0]["functionResponse"]["name"], "render_visual_check");
        assert!(g25_parts[0]["functionResponse"].get("parts").is_none());
        assert_eq!(g25_parts[1]["functionResponse"]["name"], "inspect_pages");
        assert_eq!(g25_parts[2]["inlineData"]["mimeType"], "image/png");
        assert_eq!(g25_parts[2]["inlineData"]["data"], "AQID");

        // OpenAI attaches input_image on multimodal models and omits it on text-only o3-mini:
        let openai_vis = ModelSelection { provider: ProviderKind::OpenAi, model: "gpt-5.4".into(), endpoint: None };
        assert!(supports_agent_vision(&openai_vis));
        let openai_body = crate::agent::agent_turn_body(&openai_vis, &req, false).unwrap();
        assert_eq!(openai_body["input"][3]["type"], "function_call_output");
        assert_eq!(openai_body["input"][4]["type"], "function_call_output");
        assert_eq!(openai_body["input"][5]["role"], "user");
        assert_eq!(openai_body["input"][5]["content"][1]["type"], "input_image");
        assert_eq!(openai_body["input"][5]["content"][1]["image_url"], "data:image/png;base64,AQID");

        let openai_txt = ModelSelection { provider: ProviderKind::OpenAi, model: "o3-mini".into(), endpoint: None };
        assert!(!supports_agent_vision(&openai_txt));
        let openai_txt_body = crate::agent::agent_turn_body(&openai_txt, &req, false).unwrap();
        assert_eq!(openai_txt_body["input"].as_array().unwrap().len(), 5, "text-only model must omit input_image");

        // Ollama attaches images on vision-capable models and omits them on text-only models:
        let ollama_vis = ModelSelection { provider: ProviderKind::Ollama, model: "qwen2.5vl:7b".into(), endpoint: None };
        assert!(supports_agent_vision(&ollama_vis));
        let ollama_vis_body = crate::agent::agent_turn_body(&ollama_vis, &req, false).unwrap();
        let msgs = ollama_vis_body["messages"].as_array().unwrap();
        assert_eq!(msgs.last().unwrap()["images"], json!(["AQID"]));

        let ollama_txt = ModelSelection { provider: ProviderKind::Ollama, model: "qwen2.5:7b".into(), endpoint: None };
        assert!(!supports_agent_vision(&ollama_txt));
        let ollama_txt_body = crate::agent::agent_turn_body(&ollama_txt, &req, false).unwrap();
        for m in ollama_txt_body["messages"].as_array().unwrap() {
            assert!(m.get("images").is_none(), "text-only Ollama model must omit images");
        }

        // Invalid MIME or empty bytes on ToolResultMessage.images are always rejected:
        let mut bad_req = req.clone();
        if let AgentMessage::ToolResults { results } = &mut bad_req.messages[2] {
            results[0].images[0].mime_type = "image/svg+xml".into();
        }
        assert!(crate::agent::agent_turn_body(&ollama_txt, &bad_req, false).is_err());
    }
}
