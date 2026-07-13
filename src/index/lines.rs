pub(super) fn line_starts(content: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(content.match_indices('\n').map(|(index, _)| index + 1));
    starts
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
