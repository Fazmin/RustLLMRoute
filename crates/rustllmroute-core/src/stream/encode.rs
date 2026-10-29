use bytes::{BufMut, Bytes, BytesMut};
use rustllmroute_types::{ChatChunk, GatewayError, Usage};

/// Turns normalised chat chunks into the client's wire format.
pub trait ClientEncoder: Send + 'static {
    fn chunk(&mut self, chunk: &ChatChunk) -> Vec<Bytes>;
    /// `tail` holds the withheld finish chunk(s) and, when requested, the usage chunk.
    fn finish(&mut self, tail: Vec<ChatChunk>, usage: Option<&Usage>) -> Vec<Bytes>;
    fn error(&mut self, error: &GatewayError) -> Vec<Bytes>;
    fn keepalive(&self) -> Bytes;
}

/// OpenAI chat-completions SSE: `data: {json}\n\n`, terminated by `data: [DONE]\n\n`.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenAiSseEncoder;

impl OpenAiSseEncoder {
    pub const DONE: &'static [u8] = b"data: [DONE]\n\n";
    pub const PING: &'static [u8] = b": ping\n\n";

    pub fn frame(&self, chunk: &ChatChunk) -> Bytes {
        let mut out = BytesMut::with_capacity(256);
        out.put_slice(b"data: ");
        match serde_json::to_vec(chunk) {
            Ok(json) => out.put_slice(&json),
            Err(_) => out.put_slice(b"{}"),
        }
        out.put_slice(b"\n\n");
        out.freeze()
    }

    pub fn error_frame(&self, error: &GatewayError) -> Bytes {
        let mut out = BytesMut::with_capacity(256);
        out.put_slice(b"data: ");
        out.put_slice(error.to_openai_body().to_string().as_bytes());
        out.put_slice(b"\n\n");
        out.put_slice(Self::DONE);
        out.freeze()
    }

    pub fn terminator(&self) -> Bytes {
        Bytes::from_static(Self::DONE)
    }
}

impl ClientEncoder for OpenAiSseEncoder {
    fn chunk(&mut self, chunk: &ChatChunk) -> Vec<Bytes> {
        vec![self.frame(chunk)]
    }

    fn finish(&mut self, tail: Vec<ChatChunk>, _usage: Option<&Usage>) -> Vec<Bytes> {
        let mut out: Vec<Bytes> = tail.iter().map(|chunk| self.frame(chunk)).collect();
        out.push(self.terminator());
        out
    }

    fn error(&mut self, error: &GatewayError) -> Vec<Bytes> {
        vec![self.error_frame(error)]
    }

    fn keepalive(&self) -> Bytes {
        Bytes::from_static(Self::PING)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_well_formed() {
        let chunk = ChatChunk::new("c", 1, "m");
        let frame = OpenAiSseEncoder.frame(&chunk);
        assert!(frame.starts_with(b"data: {"));
        assert!(frame.ends_with(b"}\n\n"));
        let error = OpenAiSseEncoder.error_frame(&GatewayError::internal("x"));
        assert!(error.ends_with(OpenAiSseEncoder::DONE));
        let mut encoder = OpenAiSseEncoder;
        let tail = encoder.finish(vec![chunk], None);
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[1], Bytes::from_static(OpenAiSseEncoder::DONE));
    }
}
