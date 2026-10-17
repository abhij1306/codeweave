//! A Bash run's output: one incremental UTF-8 + ANSI decoder per pipe, and a
//! bounded tail per stream with tracked character counts, so ingesting a chunk
//! costs time proportional to the chunk. Text is stripped once, at ingest.
use std::io;

/// A stream a client can read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    Combined,
    Stdout,
    Stderr,
}

impl OutputStream {
    pub fn parse(value: Option<&str>) -> io::Result<Self> {
        match value.unwrap_or("combined") {
            "combined" => Ok(Self::Combined),
            "stdout" => Ok(Self::Stdout),
            "stderr" => Ok(Self::Stderr),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown Bash output stream: {other}"),
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Combined => "combined",
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

/// A pipe the process writes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pipe {
    Stdout,
    Stderr,
}

#[derive(Debug, Default, Clone, Copy)]
enum AnsiState {
    #[default]
    Text,
    Escape,
    Csi,
    Osc,
    OscEscape,
}

/// Decodes a byte stream that may split UTF-8 sequences and ANSI escapes across
/// chunks. Invalid UTF-8 becomes U+FFFD; CSI and OSC sequences are removed.
#[derive(Debug, Default)]
pub(crate) struct StreamDecoder {
    pending_utf8: Vec<u8>,
    ansi: AnsiState,
}

impl StreamDecoder {
    /// Append the visible text of `bytes` to `out`.
    pub(crate) fn decode(&mut self, bytes: &[u8], out: &mut String) {
        self.pending_utf8.extend_from_slice(bytes);
        let mut consumed = 0;
        while consumed < self.pending_utf8.len() {
            match std::str::from_utf8(&self.pending_utf8[consumed..]) {
                Ok(valid) => {
                    strip_ansi(&mut self.ansi, valid, out);
                    consumed = self.pending_utf8.len();
                }
                Err(error) => {
                    let valid_end = consumed + error.valid_up_to();
                    if let Ok(valid) = std::str::from_utf8(&self.pending_utf8[consumed..valid_end])
                    {
                        strip_ansi(&mut self.ansi, valid, out);
                    }
                    consumed = valid_end;
                    let Some(error_len) = error.error_len() else {
                        break;
                    };
                    strip_ansi(&mut self.ansi, "\u{fffd}", out);
                    consumed += error_len;
                }
            }
        }
        self.pending_utf8.drain(..consumed);
    }

    /// Flush an incomplete trailing sequence at end of stream.
    pub(crate) fn finish(&mut self, out: &mut String) {
        let pending = String::from_utf8_lossy(&self.pending_utf8).into_owned();
        self.pending_utf8.clear();
        strip_ansi(&mut self.ansi, &pending, out);
        self.ansi = AnsiState::Text;
    }
}

/// The one ANSI escape parser. It carries state across calls.
fn strip_ansi(state: &mut AnsiState, text: &str, out: &mut String) {
    for ch in text.chars() {
        *state = match *state {
            AnsiState::Text if ch == '\u{1b}' => AnsiState::Escape,
            AnsiState::Text => {
                out.push(ch);
                AnsiState::Text
            }
            AnsiState::Escape if ch == '[' => AnsiState::Csi,
            AnsiState::Escape if ch == ']' => AnsiState::Osc,
            AnsiState::Escape if ch == '\u{1b}' => AnsiState::Escape,
            AnsiState::Escape => AnsiState::Text,
            AnsiState::Csi if ('@'..='~').contains(&ch) => AnsiState::Text,
            AnsiState::Csi => AnsiState::Csi,
            AnsiState::Osc if ch == '\u{7}' => AnsiState::Text,
            AnsiState::Osc if ch == '\u{1b}' => AnsiState::OscEscape,
            AnsiState::Osc => AnsiState::Osc,
            AnsiState::OscEscape if ch == '\\' => AnsiState::Text,
            AnsiState::OscEscape if ch == '\u{1b}' => AnsiState::OscEscape,
            AnsiState::OscEscape => AnsiState::Osc,
        };
    }
}

/// The last `limit` characters of a stream, with the count of characters
/// dropped from its front. Offsets are absolute: character `n` of the stream
/// keeps offset `n` however much is later dropped.
#[derive(Debug)]
pub(crate) struct TailBuffer {
    text: String,
    /// Byte offset in `text` where the retained tail begins.
    start: usize,
    retained_chars: usize,
    dropped_chars: usize,
    limit: usize,
}

/// One page of a [`TailBuffer`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Page<'a> {
    pub(crate) text: &'a str,
    /// Absolute offset of the next page, if more text is retained.
    pub(crate) next: Option<usize>,
}

impl TailBuffer {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            text: String::new(),
            start: 0,
            retained_chars: 0,
            dropped_chars: 0,
            limit,
        }
    }

    /// Append `text`, dropping from the front past the limit. Costs
    /// O(`text`) amortized: the dead prefix is compacted only once it is at
    /// least half the buffer.
    pub(crate) fn push(&mut self, text: &str) {
        self.text.push_str(text);
        self.retained_chars += text.chars().count();
        if self.retained_chars <= self.limit {
            return;
        }
        let excess = self.retained_chars - self.limit;
        let advance = self.text[self.start..]
            .char_indices()
            .nth(excess)
            .map_or(self.text.len() - self.start, |(index, _)| index);
        self.start += advance;
        self.retained_chars -= excess;
        self.dropped_chars += excess;
        if self.start > self.text.len() / 2 {
            self.text.drain(..self.start);
            self.start = 0;
        }
    }

    pub(crate) fn retained(&self) -> &str {
        &self.text[self.start..]
    }

    pub(crate) fn retained_chars(&self) -> usize {
        self.retained_chars
    }

    pub(crate) fn dropped_chars(&self) -> usize {
        self.dropped_chars
    }

    pub(crate) fn total_chars(&self) -> usize {
        self.dropped_chars + self.retained_chars
    }

    /// Up to `max_chars` characters from absolute offset `from`. An offset that
    /// was already dropped starts at the oldest retained character.
    pub(crate) fn page(&self, from: usize, max_chars: usize) -> Page<'_> {
        let retained = self.retained();
        let skip = from
            .saturating_sub(self.dropped_chars)
            .min(self.retained_chars);
        let begin = byte_at_char(retained, skip);
        let rest = &retained[begin..];
        let end = byte_at_char(rest, max_chars);
        let next =
            (end < rest.len()).then(|| self.dropped_chars + skip + rest[..end].chars().count());
        Page {
            text: &rest[..end],
            next,
        }
    }
}

/// Byte index of character `chars` in `value`, or its length.
fn byte_at_char(value: &str, chars: usize) -> usize {
    value
        .char_indices()
        .nth(chars)
        .map_or(value.len(), |(index, _)| index)
}

/// The first or last `limit` characters of `value`.
pub(crate) fn clamp_chars(value: &str, limit: usize, keep_tail: bool) -> Option<&str> {
    let count = value.chars().count();
    if count <= limit {
        return None;
    }
    Some(if keep_tail {
        &value[byte_at_char(value, count - limit)..]
    } else {
        &value[..byte_at_char(value, limit)]
    })
}

/// Everything a run printed: stdout, stderr, and both interleaved by chunk.
#[derive(Debug)]
pub(crate) struct RunOutput {
    stdout: TailBuffer,
    stderr: TailBuffer,
    combined: TailBuffer,
    stdout_decoder: StreamDecoder,
    stderr_decoder: StreamDecoder,
    scratch: String,
}

impl RunOutput {
    /// Each stream keeps at most `limit` characters.
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            stdout: TailBuffer::new(limit),
            stderr: TailBuffer::new(limit),
            combined: TailBuffer::new(limit),
            stdout_decoder: StreamDecoder::default(),
            stderr_decoder: StreamDecoder::default(),
            scratch: String::new(),
        }
    }

    /// Decode and retain a chunk read from `pipe`.
    pub(crate) fn ingest(&mut self, pipe: Pipe, bytes: &[u8]) {
        let mut text = std::mem::take(&mut self.scratch);
        text.clear();
        self.decoder(pipe).decode(bytes, &mut text);
        self.append(pipe, &text);
        self.scratch = text;
    }

    /// Flush `pipe`'s decoder at end of stream.
    pub(crate) fn finish(&mut self, pipe: Pipe) {
        let mut text = String::new();
        self.decoder(pipe).finish(&mut text);
        self.append(pipe, &text);
    }

    pub(crate) fn stream(&self, stream: OutputStream) -> &TailBuffer {
        match stream {
            OutputStream::Combined => &self.combined,
            OutputStream::Stdout => &self.stdout,
            OutputStream::Stderr => &self.stderr,
        }
    }

    /// Whether any stream dropped characters.
    pub(crate) fn truncated(&self) -> bool {
        // Combined receives every character either pipe does, so it drops first.
        self.combined.dropped_chars() > 0
    }

    fn decoder(&mut self, pipe: Pipe) -> &mut StreamDecoder {
        match pipe {
            Pipe::Stdout => &mut self.stdout_decoder,
            Pipe::Stderr => &mut self.stderr_decoder,
        }
    }

    fn append(&mut self, pipe: Pipe, text: &str) {
        if text.is_empty() {
            return;
        }
        match pipe {
            Pipe::Stdout => self.stdout.push(text),
            Pipe::Stderr => self.stderr.push(text),
        }
        self.combined.push(text);
    }
}

/// `bash:{run_id}:{stream}:{offset}`, where `offset` is an absolute character
/// offset in the stream.
pub(crate) fn continuation_token(run_id: &str, stream: OutputStream, offset: usize) -> String {
    format!("bash:{run_id}:{}:{offset}", stream.as_str())
}

/// The offset in a continuation token for this run and stream. The older
/// `bash:{run_id}:{offset}` form is accepted for the combined stream.
pub(crate) fn continuation_offset(
    token: Option<&str>,
    run_id: &str,
    stream: OutputStream,
) -> Option<usize> {
    let value = token?;
    let prefix = format!("bash:{run_id}:{}:", stream.as_str());
    if let Some(offset) = value.strip_prefix(&prefix) {
        return offset.parse::<usize>().ok();
    }
    if stream == OutputStream::Combined {
        return value
            .strip_prefix(&format!("bash:{run_id}:"))
            .and_then(|offset| offset.parse::<usize>().ok());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(decoder: &mut StreamDecoder, bytes: &[u8]) -> String {
        let mut out = String::new();
        decoder.decode(bytes, &mut out);
        out
    }

    fn finish(decoder: &mut StreamDecoder) -> String {
        let mut out = String::new();
        decoder.finish(&mut out);
        out
    }

    #[test]
    fn decoder_preserves_split_utf8_and_ansi_state_per_stream() {
        let mut stdout = StreamDecoder::default();
        let mut stderr = StreamDecoder::default();

        assert_eq!(decode(&mut stdout, &[b'a', 0xc3]), "a");
        assert_eq!(decode(&mut stderr, b"err\x1b[3"), "err");
        assert_eq!(decode(&mut stdout, &[0xa9, 0x1b, b'[', b'3']), "é");
        assert_eq!(decode(&mut stderr, b"1mred\x1b[0m"), "red");
        assert_eq!(decode(&mut stdout, b"2mgreen\x1b[0m"), "green");
        assert_eq!(finish(&mut stdout), "");
        assert_eq!(finish(&mut stderr), "");
    }

    #[test]
    fn decoder_strips_csi_osc_and_a_doubled_escape() {
        let mut decoder = StreamDecoder::default();
        assert_eq!(
            decode(
                &mut decoder,
                b"a\x1b]0;title\x07b\x1b\x1b[31mc\x1b]8;;x\x1b\\d"
            ),
            "abcd"
        );
    }

    #[test]
    fn decoder_replaces_invalid_bytes_and_flushes_incomplete_utf8_at_eof() {
        let mut decoder = StreamDecoder::default();
        assert_eq!(decode(&mut decoder, &[b'x', 0xff, b'y']), "x\u{fffd}y");
        assert_eq!(decode(&mut decoder, &[0xe2, 0x82]), "");
        assert_eq!(finish(&mut decoder), "\u{fffd}");
    }

    #[test]
    fn tail_buffer_keeps_the_last_characters_and_counts_the_dropped_ones() {
        let mut buffer = TailBuffer::new(4);
        buffer.push("abc");
        buffer.push("déf");

        assert_eq!(buffer.retained(), "cdéf");
        assert_eq!(
            [
                buffer.retained_chars(),
                buffer.dropped_chars(),
                buffer.total_chars()
            ],
            [4, 2, 6]
        );
    }

    #[test]
    fn tail_buffer_pages_by_absolute_offset() {
        let mut buffer = TailBuffer::new(4);
        buffer.push("abcdéf");

        assert_eq!(
            buffer.page(0, 3),
            Page {
                text: "cdé",
                next: Some(5)
            }
        );
        assert_eq!(
            buffer.page(5, 3),
            Page {
                text: "f",
                next: None
            }
        );
        assert_eq!(
            buffer.page(9, 3),
            Page {
                text: "",
                next: None
            }
        );
    }

    #[test]
    fn large_output_retains_an_exact_tail_across_split_sequences() {
        // 15 bytes per line, five visible characters: "éxyz\n". 8 KiB chunks
        // split both the two-byte "é" and the escape sequences.
        let line = "é\u{1b}[31mxyz\u{1b}[0m\n".as_bytes();
        let lines = 1_200_000;
        let bytes = line.repeat(lines);
        let mut output = RunOutput::new(1_000);

        for chunk in bytes.chunks(8 * 1024) {
            output.ingest(Pipe::Stdout, chunk);
        }
        output.finish(Pipe::Stdout);

        let stdout = output.stream(OutputStream::Stdout);
        assert_eq!(stdout.retained(), "éxyz\n".repeat(200));
        assert_eq!(stdout.dropped_chars(), 5 * lines - 1_000);
        assert_eq!(
            output.stream(OutputStream::Combined).retained(),
            stdout.retained()
        );
        assert_eq!(output.stream(OutputStream::Stderr).total_chars(), 0);
        assert!(output.truncated());
    }

    #[test]
    fn combined_interleaves_chunks_from_both_pipes() {
        let mut output = RunOutput::new(100);
        output.ingest(Pipe::Stdout, b"out1 ");
        output.ingest(Pipe::Stderr, b"err ");
        output.ingest(Pipe::Stdout, b"out2");

        assert_eq!(
            output.stream(OutputStream::Combined).retained(),
            "out1 err out2"
        );
        assert_eq!(output.stream(OutputStream::Stdout).retained(), "out1 out2");
        assert_eq!(output.stream(OutputStream::Stderr).retained(), "err ");
        assert!(!output.truncated());
    }

    #[test]
    fn clamp_keeps_the_head_or_the_tail() {
        assert_eq!(clamp_chars("abcdé", 2, false), Some("ab"));
        assert_eq!(clamp_chars("abcdé", 2, true), Some("dé"));
        assert_eq!(clamp_chars("abc", 3, true), None);
    }

    #[test]
    fn continuation_offsets_are_scoped_to_run_and_stream() {
        assert_eq!(
            continuation_offset(Some("bash:run_1:stdout:12"), "run_1", OutputStream::Stdout),
            Some(12)
        );
        assert_eq!(
            continuation_offset(Some("bash:run_1:7"), "run_1", OutputStream::Combined),
            Some(7)
        );
        assert_eq!(
            continuation_offset(Some("bash:run_1:7"), "run_1", OutputStream::Stdout),
            None
        );
        assert_eq!(
            continuation_offset(Some("bash:run_2:stdout:12"), "run_1", OutputStream::Stdout),
            None
        );
        assert_eq!(
            continuation_token("run_1", OutputStream::Stderr, 3),
            "bash:run_1:stderr:3"
        );
    }
}
