//! Preprocessing for FiveM Lua syntax extensions (backtick JOAAT hash literals).
//!
//! Outside strings and comments, single-line backtick literals like `` `adder` ``
//! are rewritten into parenthesized signed 32-bit decimal integers matching
//! [`sixm_core::joaat`] while preserving every newline in the file.

use std::borrow::Cow;

use sixm_core::joaat;

/// Rewrites single-line `` `...` `` hash literals in Lua source outside strings and
/// comments into signed 32-bit integer literals such as `(-1216765807)`.
///
/// Line numbers are preserved verbatim. Returns `Cow::Borrowed` when no hash
/// literals are present or the input is precompiled bytecode (`\x1b`).
pub(crate) fn lua_backticks(source: &[u8]) -> Cow<'_, [u8]> {
    if source.starts_with(b"\x1b") || !source.contains(&b'`') {
        return Cow::Borrowed(source);
    }

    let mut out: Option<Vec<u8>> = None;
    let mut copied_until = 0usize;
    let mut i = 0usize;
    let len = source.len();

    while i < len {
        let byte = source[i];
        match byte {
            b'-' if source.get(i + 1) == Some(&b'-') => {
                i += 2;
                if let Some((eqs, body_start)) = opening_long_bracket(source, i) {
                    i = skip_long_bracket(source, body_start, eqs);
                } else {
                    while i < len && source[i] != b'\n' {
                        i += 1;
                    }
                }
            }
            b'[' => {
                if let Some((eqs, body_start)) = opening_long_bracket(source, i) {
                    i = skip_long_bracket(source, body_start, eqs);
                } else {
                    i += 1;
                }
            }
            b'\'' | b'"' => {
                i = skip_quoted_string(source, i, byte);
            }
            b'`' => {
                if let Some(close) = find_closing_backtick(source, i + 1) {
                    let buf = out.get_or_insert_with(|| Vec::with_capacity(len + 16));
                    buf.extend_from_slice(&source[copied_until..i]);
                    let hash = signed_joaat(&source[i + 1..close]);
                    buf.extend_from_slice(format!("({hash})").as_bytes());
                    i = close + 1;
                    copied_until = i;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }

    match out {
        Some(mut buf) => {
            buf.extend_from_slice(&source[copied_until..]);
            Cow::Owned(buf)
        }
        None => Cow::Borrowed(source),
    }
}

#[expect(
    clippy::cast_possible_wrap,
    reason = "hash literals use the signed 32-bit value of GetHashKey"
)]
fn signed_joaat(text: &[u8]) -> i32 {
    joaat(text) as i32
}

fn find_closing_backtick(source: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    while i < source.len() {
        match source[i] {
            b'`' => return Some(i),
            b'\n' | b'\r' => return None,
            _ => i += 1,
        }
    }
    None
}

fn skip_quoted_string(source: &[u8], open: usize, quote: u8) -> usize {
    let mut i = open + 1;
    while i < source.len() {
        let byte = source[i];
        if byte == quote {
            return i + 1;
        }
        if byte == b'\\' {
            i = (i + 2).min(source.len());
            continue;
        }
        if byte == b'\n' || byte == b'\r' {
            return i + 1;
        }
        i += 1;
    }
    source.len()
}

fn opening_long_bracket(source: &[u8], start: usize) -> Option<(usize, usize)> {
    if source.get(start) != Some(&b'[') {
        return None;
    }
    let mut i = start + 1;
    while source.get(i) == Some(&b'=') {
        i += 1;
    }
    if source.get(i) == Some(&b'[') {
        Some((i - start - 1, i + 1))
    } else {
        None
    }
}

fn skip_long_bracket(source: &[u8], body_start: usize, eqs: usize) -> usize {
    let mut i = body_start;
    while i < source.len() {
        if source[i] == b']'
            && let Some(end) = match_closing_long_bracket(source, i, eqs)
        {
            return end;
        }
        i += 1;
    }
    source.len()
}

fn match_closing_long_bracket(source: &[u8], start: usize, eqs: usize) -> Option<usize> {
    let end_eqs = start + 1 + eqs;
    let tail = source.get(start + 1..=end_eqs)?;
    if tail[..eqs].iter().all(|b| *b == b'=') && tail[eqs] == b']' {
        Some(end_eqs + 1)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::lua_backticks;

    fn eval_text(input: &str) -> String {
        String::from_utf8(lua_backticks(input.as_bytes()).into_owned()).expect("utf8")
    }

    #[test]
    fn rewrites_hash_literals_and_preserves_newlines() {
        let src = "local a = `adder`\nlocal b = `ADDER`\nreturn a, b\n";
        let out = eval_text(src);
        assert_eq!(
            out,
            "local a = (-1216765807)\nlocal b = (-1216765807)\nreturn a, b\n"
        );
        assert_eq!(src.lines().count(), out.lines().count());
    }

    #[test]
    fn leaves_comments_and_strings_alone() {
        let src = concat!(
            "local s1 = \"`adder`\"\n",
            "local s2 = '`adder` \\'`sultan`\\''\n",
            "local s3 = [[`adder`]]\n",
            "local s4 = [=[ [`adder`] ]=]\n",
            "-- line comment `adder`\n",
            "--[[ block\ncomment `adder` ]] local x = `adder`\n",
        );
        let out = eval_text(src);
        assert!(out.contains("\"`adder`\""));
        assert!(out.contains("'`adder` \\'`sultan`\\''"));
        assert!(out.contains("[[`adder`]]"));
        assert!(out.contains("[=[ [`adder`] ]=]"));
        assert!(out.contains("-- line comment `adder`"));
        assert!(out.contains("comment `adder` ]] local x = (-1216765807)"));
    }

    #[test]
    fn avoids_token_merging_and_double_dash_comments() {
        assert_eq!(eval_text("return`adder`"), "return(-1216765807)");
        assert_eq!(eval_text("1-`adder`"), "1-(-1216765807)");
        assert_eq!(
            eval_text("local t = {[`adder`] = 1}"),
            "local t = {[(-1216765807)] = 1}"
        );
    }

    #[test]
    fn unclosed_backtick_is_left_untouched() {
        let src = "local x = `adder\nlocal y = `sultan`\n";
        let out = eval_text(src);
        assert!(out.starts_with("local x = `adder\n"));
        assert!(out.contains("local y = (970598228)"));
    }
}
