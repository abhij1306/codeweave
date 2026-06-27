pub(super) fn fit_excerpt(
    content: &str,
    start_line: usize,
    proposed_end: usize,
    max_chars: usize,
) -> (String, usize) {
    let mut end_line = proposed_end.max(start_line);
    loop {
        let excerpt = slice_lines(content, start_line, end_line);
        if excerpt.len() <= max_chars {
            return (excerpt, end_line);
        }
        if end_line == start_line {
            let mut end = max_chars.min(excerpt.len());
            while end > 0 && !excerpt.is_char_boundary(end) {
                end -= 1;
            }
            return (excerpt[..end].to_owned(), end_line);
        }
        end_line -= 1;
    }
}

pub(super) fn excerpt_lines_with_count(
    line: usize,
    total_lines: usize,
    radius: usize,
) -> (usize, usize) {
    (
        line.saturating_sub(radius).max(1),
        (line + radius).min(total_lines.max(1)),
    )
}

pub(super) fn line_starts(content: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(content.match_indices('\n').map(|(index, _)| index + 1));
    starts
}

pub(super) fn byte_to_line(line_starts: &[usize], byte_offset: usize) -> usize {
    let offset = byte_offset.min(*line_starts.last().unwrap_or(&0));
    line_starts.partition_point(|start| *start <= offset).max(1)
}

pub(super) fn line_start_byte(content: &str, line: usize) -> usize {
    if line <= 1 {
        return 0;
    }
    content
        .match_indices('\n')
        .nth(line.saturating_sub(2))
        .map(|(index, _)| index + 1)
        .unwrap_or(0)
}

pub fn slice_lines(content: &str, start_line: usize, end_line: usize) -> String {
    let start = start_line.max(1);
    let end = end_line.max(start);
    let mut output = String::new();
    for (index, line) in content.lines().enumerate() {
        let line_number = index + 1;
        if line_number < start {
            continue;
        }
        if line_number > end {
            break;
        }
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(line);
    }
    output
}

pub(super) fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}
