# Native AI provider adapters

L1 transport depends on the portable L0 `designcraft-ai-types` contract. Public
entry points are `generate`, `generate_images`, `list_models`, `test_connection`,
and `validate_endpoint`. Native workers supply the selected model and an
in-memory key; this crate never reads secret files or stores credentials. WASM
builds expose the same contracts and return an explicit native-only error.

Text generation uses OpenAI Responses JSON Schema (`strict:false` for flexible
engine command params), Gemini `generateContent` JSON Schema, and Ollama
`/api/chat` with `stream:false` and a JSON Schema `format`. Reference images use
OpenAI data URL input images, Gemini inline data, or Ollama base64 images. The
selected model must support that capability; requests never silently switch
models or providers.

Successful parsed text responses preserve measured usage even for provider
refusals, incomplete responses, or missing text. `GenerationResponse.error`
contains a generic status message in those cases; record usage before stopping.
Invalid model-generated JSON is returned as raw `text` with `error:None` so the
worker can account for the attempt and apply its bounded repair policy.
`ImageResponse.error` follows the same usage-first contract: refused/incomplete
image responses and invalid/missing image contents return empty images with a
generic error while retaining reported usage. HTTP and malformed response-JSON
errors remain transport failures with unknown measurements.

Image generation uses OpenAI `/images/generations` inline `b64_json`, Gemini
`generateContent` inline image parts, or a user-supplied ComfyUI API workflow.
OpenAI size/quality options are optional; Gemini uses its selected model defaults.
URL-only image responses are rejected; this avoids fetching arbitrary returned
URLs. PNG/JPEG/WebP signatures are recognized; document image import must still
validate decoding, dimensions, and pixel limits before embedding the asset.

For `generate_images`, ComfyUI workflows must include `{{prompt}}` in a JSON
string value. When a model is selected, they must also include `{{model}}` in a
string value such as the installed checkpoint's input. Placeholders can occur
inside nested objects/arrays or longer strings. Object keys are unchanged. Only
markers in the original workflow are substituted; markers within the task text
remain literal. Expansion is bounded by the JSON request limit. Missing bindings
fail before HTTP submission; no graph node IDs are guessed. The low-level
`generate_comfy` API executes a fully bound workflow supplied by its caller.

Limits: 4 MiB JSON requests/text responses; 24 MiB cloud image JSON responses;
16 MiB per returned image, 32 MiB combined and at most 8 images; reference images
at most 8 and 2 MiB combined; JSON depth 64 and 100,000 values. Native calls have
a 10-second connect limit and 90-second overall HTTP limit. Cancellation is
checked before/after HTTP and between read chunks. Blocking HTTP cannot abort
an in-flight socket immediately; the worker must discard cancelled results.
ComfyUI polling is separately bounded to 600 seconds, with short cancellable
sleeps. Cancelling or failing a ComfyUI job deletes only its own pending queue
entry; already running server-side work may continue. `/interrupt` is never used.

Cloud endpoints require HTTPS. Local HTTP is accepted only for loopback
addresses/localhost. Endpoint userinfo, query, and fragments are rejected;
redirects are refused. Authentication is in sensitive headers. Error responses
and reqwest errors are never echoed because they can contain keys or prompts.

Usage contains optional actual measurements, including reported cache hits and
reasoning. Output counts include Gemini thoughts; reasoning is a subset. Missing
measurements (including cache writes and image-specific billing dimensions) stay
unknown. This crate neither estimates prices nor invents zero-cost generations.
Connection tests list models/checkpoints and do not spend a generation. Discovery
does not prove vision, image generation, or every JSON Schema is supported.

Official references checked while implementing:

- [OpenAI Responses](https://platform.openai.com/docs/api-reference/responses/create)
- [OpenAI structured output](https://platform.openai.com/docs/guides/structured-outputs)
- [OpenAI image generation](https://developers.openai.com/api/docs/guides/image-generation)
- [Gemini generateContent and usage](https://ai.google.dev/api/generate-content)
- [Gemini model discovery](https://ai.google.dev/api/models)
- [Gemini image generation](https://ai.google.dev/gemini-api/docs/image-generation)
- [Ollama chat](https://docs.ollama.com/api/chat)
- [ComfyUI server routes](https://docs.comfy.org/development/comfyui-server/comms_routes)

Tests use synthetic fixtures/local mock servers. No real keys or paid API calls
are required or made by the test suite.
