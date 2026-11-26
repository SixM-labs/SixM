//! Console line tokenizer and `+command` argument splitting.

use sixm_server::{ArgError, Invocation, TokenError, parse_args, tokenize};

fn words(line: &str) -> Vec<String> {
    tokenize(line).unwrap_or_else(|err| panic!("`{line}`: {err}"))
}

#[test]
fn splits_on_any_whitespace() {
    assert_eq!(
        words("  start \t hello   world "),
        ["start", "hello", "world"]
    );
}

#[test]
fn quoted_strings_keep_their_spaces() {
    assert_eq!(
        words(r#"set sv_hostname "My Server""#),
        ["set", "sv_hostname", "My Server"]
    );
}

#[test]
fn escaped_quote_inside_quotes_is_literal() {
    assert_eq!(words(r#"say "he said \"hi\"""#), ["say", "he said \"hi\""]);
}

#[test]
fn backslash_outside_an_escape_is_literal() {
    assert_eq!(
        words(r#"exec "C:\cfg\server.cfg""#),
        ["exec", r"C:\cfg\server.cfg"]
    );
}

#[test]
fn empty_quoted_string_is_a_word() {
    assert_eq!(words(r#"set empty """#), ["set", "empty", ""]);
}

#[test]
fn quote_can_join_part_of_a_word() {
    assert_eq!(words(r#"a"b c"d e"#), ["ab cd", "e"]);
}

#[test]
fn hash_and_double_slash_start_comments_outside_quotes() {
    assert_eq!(words("set a 1 # note"), ["set", "a", "1"]);
    assert_eq!(words("set b 2 // note"), ["set", "b", "2"]);
    assert_eq!(words("set c3 # ignored"), ["set", "c3"]);
    assert_eq!(words("set d a#b"), ["set", "d", "a"]);
}

#[test]
fn comment_markers_inside_quotes_are_text() {
    assert_eq!(words(r#"set e "x # y // z""#), ["set", "e", "x # y // z"]);
}

#[test]
fn blank_and_comment_only_lines_have_no_words() {
    assert_eq!(words(""), Vec::<String>::new());
    assert_eq!(words("   \t  "), Vec::<String>::new());
    assert_eq!(words("# only a comment"), Vec::<String>::new());
    assert_eq!(words("// only a comment"), Vec::<String>::new());
}

#[test]
fn unterminated_quote_is_an_error() {
    assert_eq!(
        tokenize(r#"set x "oops"#),
        Err(TokenError::UnterminatedQuote)
    );
}

#[test]
fn plus_arguments_split_into_commands() {
    let args = ["+exec", "server.cfg", "+set", "sv_hostname", "My Server"].map(str::to_owned);
    assert_eq!(
        parse_args(args),
        Ok(Invocation::Run(vec![
            vec!["exec".to_owned(), "server.cfg".to_owned()],
            vec![
                "set".to_owned(),
                "sv_hostname".to_owned(),
                "My Server".to_owned(),
            ],
        ]))
    );
}

#[test]
fn no_arguments_means_no_commands() {
    assert_eq!(parse_args(Vec::new()), Ok(Invocation::Run(Vec::new())));
}

#[test]
fn help_and_version_win_over_commands() {
    let help = ["+exec", "x.cfg", "--help"].map(str::to_owned);
    assert_eq!(parse_args(help), Ok(Invocation::Help));
    let version = ["--version"].map(str::to_owned);
    assert_eq!(parse_args(version), Ok(Invocation::Version));
}

#[test]
fn argument_before_any_command_is_rejected() {
    let args = ["server.cfg", "+quit"].map(str::to_owned);
    assert_eq!(
        parse_args(args),
        Err(ArgError::NotACommand("server.cfg".to_owned()))
    );
}

#[test]
fn bare_plus_is_rejected() {
    let args = ["+"].map(str::to_owned);
    assert_eq!(parse_args(args), Err(ArgError::EmptyCommand));
}
