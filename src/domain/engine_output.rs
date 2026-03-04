pub const STDOUT_FILE_MARKER_PREFIX: &str = "__PRISMFLOW_STDOUT_FILE__=";

pub fn extract_stdout_file_marker(output: &str) -> Option<String> {
    stdout_marker_span(output).map(|(path, _, _)| path)
}

pub fn strip_stdout_file_marker(output: &str) -> String {
    if let Some((_, start, end)) = stdout_marker_span(output) {
        let mut out = String::with_capacity(output.len().saturating_sub(end - start));
        out.push_str(&output[..start]);
        out.push_str(&output[end..]);
        return out.trim().to_string();
    }
    output.to_string()
}

fn stdout_marker_span(output: &str) -> Option<(String, usize, usize)> {
    let start = output.find(STDOUT_FILE_MARKER_PREFIX)?;
    let path_start = start + STDOUT_FILE_MARKER_PREFIX.len();
    let mut end = output.len();
    for (offset, ch) in output[path_start..].char_indices() {
        if ch.is_whitespace() {
            end = path_start + offset;
            break;
        }
    }
    if end <= path_start {
        return None;
    }
    let path = output[path_start..end].trim().to_string();
    if path.is_empty() {
        return None;
    }
    Some((path, start, end))
}

#[cfg(test)]
mod tests {
    use super::{STDOUT_FILE_MARKER_PREFIX, extract_stdout_file_marker, strip_stdout_file_marker};

    #[test]
    fn extract_supports_prefixed_text() {
        let s = format!("prefix\n\n{}D:/tmp/stdout.log", STDOUT_FILE_MARKER_PREFIX);
        assert_eq!(
            extract_stdout_file_marker(&s).as_deref(),
            Some("D:/tmp/stdout.log")
        );
    }

    #[test]
    fn strip_removes_marker_only() {
        let s = format!("hello {}D:/tmp/stdout.log world", STDOUT_FILE_MARKER_PREFIX);
        assert_eq!(strip_stdout_file_marker(&s), "hello  world");
    }
}
