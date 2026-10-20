//! Just enough shell to read a command line and to write one back safely.
//!
//! This is not a shell parser. Classification only needs the simple commands
//! inside a line (`cd web && pnpm test | tee log` holds `cd web`, `pnpm test`
//! and `tee log`) with their quotes removed. Anything fancier — `$(…)`,
//! here-docs, functions — is read as plain words, and the worst outcome of
//! that is a command classified as light, which runs ungated as it would
//! without Headroom.

/// Split a command line into simple commands, each a list of words with
/// quotes and backslash escapes resolved.
pub fn simple_commands(line: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars().peekable();

    let end_word = |word: &mut String, in_word: &mut bool, words: &mut Vec<String>| {
        if *in_word {
            words.push(std::mem::take(word));
            *in_word = false;
        }
    };

    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    word.push(q);
                }
            }
            '"' => {
                in_word = true;
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' => {
                            if let Some(&n) = chars.peek()
                                && matches!(n, '"' | '\\' | '$' | '`')
                            {
                                word.push(n);
                                chars.next();
                                continue;
                            }
                            word.push('\\');
                        }
                        _ => word.push(q),
                    }
                }
            }
            '\\' => match chars.next() {
                // A backslash-newline is a line continuation, not a word.
                Some('\n') => {}
                n => {
                    in_word = true;
                    word.push(n.unwrap_or('\\'));
                }
            },
            ';' | '&' | '|' | '\n' | '(' | ')' => {
                end_word(&mut word, &mut in_word, &mut words);
                if !words.is_empty() {
                    out.push(std::mem::take(&mut words));
                }
            }
            c if c.is_whitespace() => end_word(&mut word, &mut in_word, &mut words),
            _ => {
                in_word = true;
                word.push(c);
            }
        }
    }
    end_word(&mut word, &mut in_word, &mut words);
    if !words.is_empty() {
        out.push(words);
    }
    out
}

/// Quote one word for a POSIX shell. Plain words stay readable; anything
/// else is single-quoted, with embedded single quotes spliced as `'\''`.
pub fn quote(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c));
    if plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// Join words into one command line that a shell splits back into the same words.
pub fn join(words: &[String]) -> String {
    words.iter().map(|w| quote(w)).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sc(s: &str) -> Vec<Vec<String>> {
        simple_commands(s)
    }

    fn ex(cmds: &[&[&str]]) -> Vec<Vec<String>> {
        cmds.iter()
            .map(|c| c.iter().map(|w| w.to_string()).collect())
            .collect()
    }

    #[test]
    fn splits_on_every_operator() {
        assert_eq!(
            sc("cd web && pnpm test | tee log; echo done || true & wait"),
            ex(&[
                &["cd", "web"],
                &["pnpm", "test"],
                &["tee", "log"],
                &["echo", "done"],
                &["true"],
                &["wait"],
            ])
        );
    }

    #[test]
    fn quotes_group_and_disappear() {
        assert_eq!(
            sc(r#"vitest run "src/a b.test.ts" 'it'\''s' x\ y"#),
            ex(&[&["vitest", "run", "src/a b.test.ts", "it's", "x y"]])
        );
    }

    #[test]
    fn operators_inside_quotes_are_text() {
        assert_eq!(
            sc("echo 'a && b' \"c | d\""),
            ex(&[&["echo", "a && b", "c | d"]])
        );
    }

    #[test]
    fn empty_and_blank_lines_hold_no_commands() {
        assert!(simple_commands("").is_empty());
        assert!(simple_commands("  \n ;; && ").is_empty());
    }

    #[test]
    fn empty_quotes_are_an_empty_word() {
        assert_eq!(sc("printf ''"), ex(&[&["printf", ""]]));
    }

    #[test]
    fn unterminated_quote_takes_the_rest_of_the_line() {
        assert_eq!(sc("echo 'oops && x"), ex(&[&["echo", "oops && x"]]));
    }

    #[test]
    fn line_continuation_joins_lines() {
        assert_eq!(sc("pnpm \\\n  test"), ex(&[&["pnpm", "test"]]));
    }

    #[test]
    fn quote_round_trips_through_the_splitter() {
        for w in [
            "plain", "", "it's", "a b", "$HOME", "x\ny", "'''", "--flag=1", "\\",
        ] {
            let line = format!("echo {}", quote(w));
            assert_eq!(
                simple_commands(&line),
                vec![vec!["echo".to_string(), w.to_string()]],
                "{w:?}"
            );
        }
    }

    #[test]
    fn join_keeps_plain_words_readable() {
        let w: Vec<String> = ["pnpm", "vitest", "run", "src/a.test.ts"]
            .map(String::from)
            .to_vec();
        assert_eq!(join(&w), "pnpm vitest run src/a.test.ts");
    }
}
