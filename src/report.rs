//! `dawonai report`: sending DawonAI the text its safety check refused, when the person thinks the
//! refusal was wrong.
//!
//! pai tells the model API it handles reports (`x-pai-report: 1`). A refusal then carries the
//! request's latest turn (the user and tool messages after the model's last answer, as `parts`),
//! with a tag only the model server can make: in the error body
//! (`dawonai_report`) or, when streamed, as an SSE comment (`: dawonai-report {...}`). pai takes it
//! out before the coding harness sees the response and keeps the latest one in memory only.
//!
//! When the person's latest message is `dawonai report`, pai doesn't send that request to the
//! model. It sends the kept text to `/v1/safety/report` with the day pass and answers the harness
//! itself. Nothing is sent unless the person types it.

use std::time::{Duration, Instant};

use serde_json::Value;

pub const HEADER: &str = "x-pai-report";
pub const FIELD: &str = "dawonai_report";
const SSE_PREFIX: &[u8] = b": dawonai-report ";
/// What the person types.
pub const COMMAND: &str = "dawonai report";
/// A harness may send the same message again (a retry, a title request); within this long after
/// a report it gets the same answer instead of a second report.
pub const SENT_ANSWER_FOR: Duration = Duration::from_secs(600);

/// The latest refusal's report, held in memory until it's sent or replaced.
#[derive(Debug, Clone)]
pub struct Held {
    pub payload: Value,
    pub sent: Option<Instant>,
}

impl Held {
    pub fn bytes(&self) -> usize {
        let parts = self.payload.get("parts").and_then(Value::as_array);
        parts.map_or(0, |p| p.iter().filter_map(Value::as_str).map(str::len).sum())
    }
}

/// Whether the request's latest message is the person typing `dawonai report`: the last item of
/// `messages` (Anthropic, OpenAI chat) or `input` (OpenAI Responses) is the user's, and one of its
/// texts is the command, in any case, with or without backticks. A harness may add its own text
/// blocks to the same message (Claude Code's reminders), so any one text block counts.
pub fn is_report_request(body: &[u8]) -> bool {
    let Ok(v) = serde_json::from_slice::<Value>(body) else { return false };
    let last = match (v.get("messages"), v.get("input")) {
        (Some(Value::Array(m)), _) => m.last(),
        (_, Some(Value::String(s))) => return is_command(s),
        (_, Some(Value::Array(items))) => items.last(),
        _ => None,
    };
    let Some(last) = last else { return false };
    if last.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let mut texts = Vec::new();
    collect_texts(last.get("content").unwrap_or(&Value::Null), &mut texts);
    texts.iter().any(|t| is_command(t))
}

fn is_command(s: &str) -> bool {
    s.trim().trim_matches('`').trim().eq_ignore_ascii_case(COMMAND)
}

fn collect_texts<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
    match v {
        Value::String(s) => out.push(s),
        Value::Array(parts) => {
            for p in parts {
                match p {
                    Value::String(s) => out.push(s),
                    _ => {
                        if let Some(Value::String(s)) = p.get("text") {
                            out.push(s);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// Takes the report out of an error body. Returns the body without it, and the report.
pub fn take_from_body(body: &[u8]) -> (Option<Vec<u8>>, Option<Value>) {
    if !contains(body, FIELD.as_bytes()) {
        return (None, None);
    }
    let Ok(mut v) = serde_json::from_slice::<Value>(body) else { return (None, None) };
    let Some(report) = v.as_object_mut().and_then(|o| o.remove(FIELD)) else { return (None, None) };
    (Some(v.to_string().into_bytes()), Some(report))
}

/// Removes the report comment from a streamed response as it passes, keeping the report. A
/// comment split across chunks is held until its blank line arrives, and a chunk that ends with
/// what could be the start of one holds those few bytes until the next; everything else passes at
/// once.
pub struct SseFilter {
    held: Vec<u8>,
    /// Whether the next byte out starts a line.
    line_start: bool,
    pub report: Option<Value>,
}

impl Default for SseFilter {
    fn default() -> Self {
        SseFilter { held: Vec::new(), line_start: true, report: None }
    }
}

impl SseFilter {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.held.extend_from_slice(chunk);
        let mut out = Vec::new();
        loop {
            let starts_line = |held: &[u8], i: usize, line_start: bool| if i == 0 { line_start } else { held[i - 1] == b'\n' };
            let found = (0..self.held.len().saturating_sub(SSE_PREFIX.len() - 1))
                .find(|&i| self.held[i..].starts_with(SSE_PREFIX) && starts_line(&self.held, i, self.line_start));
            match found {
                Some(at) => {
                    let Some(end) = find(&self.held[at..], b"\n\n") else {
                        self.emit(at, &mut out);
                        return out;
                    };
                    if let Ok(v) = serde_json::from_slice::<Value>(&self.held[at + SSE_PREFIX.len()..at + end]) {
                        self.report = Some(v);
                    }
                    self.emit(at, &mut out);
                    self.held.drain(..end + 2);
                    self.line_start = true;
                }
                None => {
                    // Hold a tail that could be the start of the prefix.
                    let n = self.held.len();
                    let keep = (1..SSE_PREFIX.len().min(n + 1))
                        .rev()
                        .find(|&k| SSE_PREFIX.starts_with(&self.held[n - k..]) && starts_line(&self.held, n - k, self.line_start))
                        .unwrap_or(0);
                    self.emit(n - keep, &mut out);
                    return out;
                }
            }
        }
    }

    /// Moves the first `n` held bytes out.
    fn emit(&mut self, n: usize, out: &mut Vec<u8>) {
        if n > 0 {
            self.line_start = self.held[n - 1] == b'\n';
            out.extend(self.held.drain(..n));
        }
    }

    /// Whatever is still held when the stream ends.
    pub fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.held)
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    find(hay, needle).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn spots_the_command_in_each_api() {
        let yes = [
            json!({"messages": [{"role": "user", "content": "dawonai report"}]}),
            json!({"messages": [{"role": "user", "content": [{"type": "text", "text": "<system-reminder>x</system-reminder>"}, {"type": "text", "text": " DawonAI Report\n"}]}]}),
            json!({"messages": [{"role": "assistant", "content": "hi"}, {"role": "user", "content": "`dawonai report`"}]}),
            json!({"input": "dawonai report"}),
            json!({"input": [{"role": "user", "content": [{"type": "input_text", "text": "dawonai report"}]}]}),
        ];
        for v in yes {
            assert!(is_report_request(v.to_string().as_bytes()), "{v}");
        }
        let no = [
            json!({"messages": [{"role": "user", "content": "please dawonai report this"}]}),
            json!({"messages": [{"role": "user", "content": "dawonai report"}, {"role": "assistant", "content": "ok"}]}),
            json!({"messages": [{"role": "user", "content": [{"type": "tool_result", "content": "dawonai report"}]}]}),
            json!({"input": [{"type": "function_call_output", "output": "dawonai report"}]}),
            json!({"prompt": "dawonai report"}),
        ];
        for v in no {
            assert!(!is_report_request(v.to_string().as_bytes()), "{v}");
        }
    }

    #[test]
    fn takes_the_report_out_of_an_error_body() {
        let body = json!({"type": "error", "error": {"type": "refused", "message": "m"}, "dawonai_report": {"parts": ["t"], "hour": 1, "tag": "a"}});
        let (rest, report) = take_from_body(body.to_string().as_bytes());
        assert_eq!(report.unwrap()["parts"][0], "t");
        let rest: Value = serde_json::from_slice(&rest.unwrap()).unwrap();
        assert_eq!(rest, json!({"type": "error", "error": {"type": "refused", "message": "m"}}));
        assert_eq!(take_from_body(br#"{"error":{}}"#), (None, None));
    }

    #[test]
    fn removes_the_comment_from_a_stream_however_it_is_split() {
        let stream = b"event: message_start\ndata: {}\n\n: dawonai-report {\"parts\":[\"t\"],\"hour\":1,\"tag\":\"a\"}\n\nevent: error\ndata: {\"x\":1}\n\n";
        let want = b"event: message_start\ndata: {}\n\nevent: error\ndata: {\"x\":1}\n\n";
        for size in [1, 3, 7, 40, stream.len()] {
            let mut f = SseFilter::default();
            let mut out = Vec::new();
            for c in stream.chunks(size) {
                out.extend(f.push(c));
            }
            out.extend(f.finish());
            assert_eq!(String::from_utf8_lossy(&out), String::from_utf8_lossy(want), "chunks of {size}");
            assert_eq!(f.report.as_ref().unwrap()["parts"][0], "t", "chunks of {size}");
        }
        let mut f = SseFilter::default();
        assert_eq!(f.push(b"data: plain\n\n"), b"data: plain\n\n");
        assert_eq!(f.push(b": keepalive\n\n"), b": keepalive\n\n", "other comments pass");
        assert_eq!(f.push(b"data: x\n\n:"), b"data: x\n\n", "a possible start is held");
        assert_eq!(f.push(b" ping\n\n"), b": ping\n\n");
        assert!(f.report.is_none());
    }
}
