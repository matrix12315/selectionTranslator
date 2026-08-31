use crate::ProviderError;
use serde_json::Value;

pub(crate) const MAX_SSE_LINE_BYTES: usize = 64 * 1024;
pub(crate) const MAX_SSE_EVENT_BYTES: usize = 256 * 1024;

/// A decoded completion event.  `Done` is kept separate so callers cannot
/// accidentally render the protocol sentinel as user-visible output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Event {
    Delta(String),
    Done,
}

/// Incremental SSE decoder.  Bytes are retained until they form valid UTF-8,
/// so a multi-byte code point split across WinHTTP reads is never replaced or
/// lost.  The parser accepts CRLF and LF line endings and multiple `data:`
/// lines in a single SSE event.
#[derive(Default)]
pub(crate) struct Decoder {
    bytes: Vec<u8>,
    data: Vec<String>,
    data_bytes: usize,
}

impl Decoder {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<Event>, ProviderError> {
        self.bytes.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(end) = self.bytes.iter().position(|byte| *byte == b'\n') {
            if end > MAX_SSE_LINE_BYTES {
                return Err(ProviderError::ResponseTooLarge);
            }
            let mut line = self.bytes.drain(..=end).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = std::str::from_utf8(&line).map_err(|_| ProviderError::MalformedJson)?;
            self.line(line, &mut events)?;
        }
        if self.bytes.len() > MAX_SSE_LINE_BYTES {
            return Err(ProviderError::ResponseTooLarge);
        }
        Ok(events)
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<Event>, ProviderError> {
        let mut events = Vec::new();
        if !self.bytes.is_empty() {
            if self.bytes.len() > MAX_SSE_LINE_BYTES {
                return Err(ProviderError::ResponseTooLarge);
            }
            let line = std::mem::take(&mut self.bytes);
            let line = std::str::from_utf8(&line).map_err(|_| ProviderError::MalformedJson)?;
            self.line(line.trim_end_matches('\r'), &mut events)?;
        }
        self.emit(&mut events)?;
        Ok(events)
    }

    fn line(&mut self, line: &str, events: &mut Vec<Event>) -> Result<(), ProviderError> {
        if line.is_empty() {
            self.emit(events)
        } else if line.starts_with(':') {
            Ok(())
        } else if let Some(value) = line.strip_prefix("data:") {
            let value = value.strip_prefix(' ').unwrap_or(value);
            let value_bytes = value.len();
            if self
                .data_bytes
                .checked_add(value_bytes.saturating_add(1))
                .is_none_or(|size| size > MAX_SSE_EVENT_BYTES)
            {
                return Err(ProviderError::ResponseTooLarge);
            }
            self.data.push(value.to_owned());
            self.data_bytes += value_bytes.saturating_add(1);
            Ok(())
        } else {
            // SSE fields other than data are irrelevant to chat completions.
            Ok(())
        }
    }

    fn emit(&mut self, events: &mut Vec<Event>) -> Result<(), ProviderError> {
        if self.data.is_empty() {
            return Ok(());
        }
        let data = self.data.join("\n");
        self.data.clear();
        self.data_bytes = 0;
        if data == "[DONE]" {
            events.push(Event::Done);
            return Ok(());
        }
        let value: Value = serde_json::from_str(&data).map_err(|_| ProviderError::MalformedJson)?;
        let choice = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first());
        let content = choice
            .and_then(|choice| choice.get("delta"))
            .and_then(|delta| delta.get("content"));
        if let Some(content) = content {
            if let Some(text) = content.as_str() {
                if !text.is_empty() {
                    events.push(Event::Delta(text.to_owned()));
                }
            } else if !content.is_null() {
                return Err(ProviderError::MalformedJson);
            }
        }
        if let Some(reason) = choice.and_then(|choice| choice.get("finish_reason")) {
            if !reason.is_null() {
                if reason.as_str().is_none() {
                    return Err(ProviderError::MalformedJson);
                }
                // A valid chat-completion terminal event is sufficient even
                // when a compatible server omits the optional [DONE] frame.
                events.push(Event::Done);
            }
        }
        Ok(())
    }
}

pub(crate) fn parse_non_streaming(value: &[u8]) -> Result<String, ProviderError> {
    let value: Value = serde_json::from_slice(value).map_err(|_| ProviderError::MalformedJson)?;
    let content = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .ok_or(ProviderError::MalformedJson)?;
    Ok(content.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{parse_non_streaming, Decoder, Event, MAX_SSE_EVENT_BYTES, MAX_SSE_LINE_BYTES};
    use crate::ProviderError;

    #[test]
    fn decodes_frames_and_done() {
        let mut decoder = Decoder::default();
        let events = decoder
            .push(b"data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\n")
            .unwrap();
        assert_eq!(events, vec![Event::Delta("hel".to_owned())]);
        let events = decoder.push(b"data: [DONE]\n\n").unwrap();
        assert_eq!(events, vec![Event::Done]);
    }

    #[test]
    fn preserves_utf8_split_between_reads() {
        let payload = b"data: {\"choices\":[{\"delta\":{\"content\":\"\xE4\xBD\xA0\"}}]}\n\n";
        let mut decoder = Decoder::default();
        let split = payload.iter().position(|byte| *byte == 0xA0).unwrap();
        assert!(decoder.push(&payload[..split]).unwrap().is_empty());
        let events = decoder.push(&payload[split..]).unwrap();
        assert_eq!(events, vec![Event::Delta("你".to_owned())]);
    }

    #[test]
    fn preserves_data_frame_split_between_reads() {
        let mut decoder = Decoder::default();
        assert!(decoder.push(b"dat").unwrap().is_empty());
        assert!(decoder
            .push(b"a: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n")
            .unwrap()
            .is_empty());
        let events = decoder.push(b"\n").unwrap();
        assert_eq!(events, vec![Event::Delta("ok".to_owned())]);
    }

    #[test]
    fn accepts_crlf_and_non_streaming_json() {
        let mut decoder = Decoder::default();
        let events = decoder
            .push(b"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\r\n\r\n")
            .unwrap();
        assert_eq!(events, vec![Event::Delta("ok".to_owned())]);
        let output =
            parse_non_streaming(br#"{"choices":[{"message":{"content":"answer"}}]}"#).unwrap();
        assert_eq!(output, "answer");
    }

    #[test]
    fn rejects_malformed_event() {
        let mut decoder = Decoder::default();
        assert!(decoder.push(b"data: nope\n\n").is_err());
    }

    #[test]
    fn rejects_an_unbounded_line_or_event() {
        let mut line_decoder = Decoder::default();
        let oversized_line = vec![b'x'; MAX_SSE_LINE_BYTES + 1];
        assert_eq!(
            line_decoder.push(&oversized_line),
            Err(ProviderError::ResponseTooLarge)
        );

        let mut event_decoder = Decoder::default();
        let mut oversized_event = b"data: ".to_vec();
        oversized_event.extend(std::iter::repeat_n(b'x', MAX_SSE_EVENT_BYTES));
        assert_eq!(
            event_decoder.push(&oversized_event),
            Err(ProviderError::ResponseTooLarge)
        );
    }

    #[test]
    fn accepts_a_valid_finish_reason_without_done_sentinel() {
        let mut decoder = Decoder::default();
        let events = decoder
            .push(b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n")
            .unwrap();
        assert_eq!(events, vec![Event::Done]);
    }
}
