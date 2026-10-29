//! Random and mutated frames must never panic a stream decoder; errors are fine.

use std::collections::HashMap;

use proptest::prelude::*;
use rustllmroute_providers::anthropic::AnthropicSseDecoder;
use rustllmroute_providers::bedrock::ConverseStreamDecoder;
use rustllmroute_providers::gemini::GeminiSseDecoder;
use rustllmroute_providers::openai_compat::OpenAiSseDecoder;
use rustllmroute_providers::{ChunkDecoder, Frame};

const SEEDS: &[&str] = &[
    r#"{"id":"c","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"hi"}}]}"#,
    r#"{"type":"message_start","message":{"id":"m","type":"message","role":"assistant","model":"c","content":[],"stop_reason":null,"usage":{"input_tokens":1,"output_tokens":1}}}"#,
    r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"a\":"}}"#,
    r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"f","args":{}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1}}"#,
    r#"{"contentBlockIndex":1,"delta":{"toolUse":{"input":"{"}}}"#,
    r#"{"error":{"message":"x","code":500}}"#,
    "[DONE]",
];

const EVENTS: &[&str] = &[
    "message_start",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
    "message_delta",
    "message_stop",
    "error",
    "messageStart",
    "contentBlockStart",
    "contentBlockDelta",
    "messageStop",
    "metadata",
    "throttlingException",
    "ping",
];

fn mutate(seed: &str, edits: &[(usize, u8)]) -> Vec<u8> {
    let mut bytes = seed.as_bytes().to_vec();
    for (position, byte) in edits {
        if bytes.is_empty() {
            break;
        }
        let index = position % bytes.len();
        bytes[index] = *byte;
    }
    bytes
}

fn run_all(event: Option<&str>, data: &[u8]) {
    let frame = Frame { event, data };
    let mut out = Vec::new();
    let _ = OpenAiSseDecoder::new("openai").decode(frame, &mut out);
    let _ = AnthropicSseDecoder::new("anthropic", false, HashMap::new()).decode(frame, &mut out);
    let _ = GeminiSseDecoder::new("gemini", 1).decode(frame, &mut out);
    let _ = ConverseStreamDecoder::new("bedrock", false, HashMap::new(), 1).decode(frame, &mut out);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn mutated_frames_never_panic(
        seed in 0usize..SEEDS.len(),
        event in 0usize..EVENTS.len(),
        edits in proptest::collection::vec((any::<usize>(), any::<u8>()), 0..6),
    ) {
        let data = mutate(SEEDS[seed], &edits);
        run_all(Some(EVENTS[event]), &data);
        run_all(None, &data);
    }

    #[test]
    fn random_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..512), event in 0usize..EVENTS.len()) {
        run_all(Some(EVENTS[event]), &data);
    }
}

#[test]
fn sequences_of_valid_frames_stay_consistent() {
    let mut decoder = AnthropicSseDecoder::new("anthropic", false, HashMap::new());
    let mut out = Vec::new();
    for (event, data) in [
        ("message_start", SEEDS[1]),
        ("content_block_delta", SEEDS[2]),
        ("content_block_delta", SEEDS[2]),
        ("message_stop", "{\"type\":\"message_stop\"}"),
    ] {
        let _ = decoder.decode(
            Frame {
                event: Some(event),
                data: data.as_bytes(),
            },
            &mut out,
        );
    }
    assert!(decoder.is_done());
}
