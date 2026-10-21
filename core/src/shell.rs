//! Just enough shell to read a command line and to write one back safely.
//!
//! This is not a shell parser. Classification only needs the simple commands
//! inside a line (`cd web && pnpm test | tee log` holds `cd web`, `pnpm test`
//! and `tee log`) with their quotes removed. A here-doc's body is input to
//! its command, not commands, so it is skipped: a Python script fed to
//! `python3 - <<'EOF'` must not be read as shell. Anything fancier, `$(…)`
//! or functions, is read as plain words, and the worst outcome of that is a
//! command classified as light, which runs ungated as it would without
//! Headroom.

/// Split a command line into simple commands, each a list of words with
/// quotes and backslash escapes resolved.
pub fn simple_commands(line: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    // Here-docs opened on the current line: delimiter, and whether `<<-`
    // strips leading tabs. Their bodies start after the next newline.
    let mut docs: Vec<(String, bool)> = Vec::new();
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
            '<' if chars.peek() == Some(&'<') => {
                chars.next();
                end_word(&mut word, &mut in_word, &mut words);
                if chars.peek() == Some(&'<') {
                    // `<<<word`, a here-string: the word is ordinary text.
                    chars.next();
                    continue;
                }
                let strip = chars.next_if_eq(&'-').is_some();
                while chars.next_if(|c| *c == ' ' || *c == '\t').is_some() {}
                let mut delim = String::new();
                while let Some(c) = chars.next_if(|c| !c.is_whitespace() && !";&|()<>".contains(*c))
                {
                    if c != '\'' && c != '"' && c != '\\' {
                        delim.push(c);
                    }
                }
                if !delim.is_empty() {
                    docs.push((delim, strip));
                }
            }
            ';' | '&' | '|' | '\n' | '(' | ')' => {
                end_word(&mut word, &mut in_word, &mut words);
                if !words.is_empty() {
                    out.push(std::mem::take(&mut words));
                }
                if c == '\n' {
                    for (delim, strip) in docs.drain(..) {
                        // Up to and including the delimiter's own line; an
                        // unterminated body runs to the end.
                        loop {
                            let body: String = chars.by_ref().take_while(|&c| c != '\n').collect();
                            let l = if strip {
                                body.trim_start_matches('\t')
                            } else {
                                &body
                            };
                            if l == delim || chars.peek().is_none() {
                                break;
                            }
                        }
                    }
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
    fn a_here_doc_body_is_input_not_commands() {
        let script = "python3 - <<'EOF'\nx = open('w/tsc-fe-1020.txt').read()\nimport os; os.system('pnpm test')\nEOF\npnpm lint";
        assert_eq!(sc(script), ex(&[&["python3", "-"], &["pnpm", "lint"]]));
        // Bare and double-quoted delimiters, and the rest of the opening
        // line still counts.
        assert_eq!(
            sc("cat <<END | pnpm test\nrm -rf x\nEND"),
            ex(&[&["cat"], &["pnpm", "test"]])
        );
        assert_eq!(
            sc("cat <<\"X\"\nvitest\nX\necho ok"),
            ex(&[&["cat"], &["echo", "ok"]])
        );
    }

    #[test]
    fn here_doc_edge_cases() {
        // `<<-` lets the closing delimiter be indented with tabs.
        assert_eq!(
            sc("cat <<-EOF\n\tvitest\n\tEOF\necho ok"),
            ex(&[&["cat"], &["echo", "ok"]])
        );
        // Without `-`, an indented delimiter doesn't close it.
        assert_eq!(sc("cat <<EOF\n\tEOF\nvitest"), ex(&[&["cat"]]));
        // Two on one line: bodies follow in order.
        assert_eq!(
            sc("paste <<A <<B\n1\nA\n2\nB\ntsc"),
            ex(&[&["paste"], &["tsc"]])
        );
        // Unterminated: the rest is body.
        assert_eq!(sc("cat <<EOF\npnpm test"), ex(&[&["cat"]]));
        // A here-string is a word, not a here-doc.
        assert_eq!(
            sc("grep x <<< \"a b\"\npnpm test"),
            ex(&[&["grep", "x", "a b"], &["pnpm", "test"]])
        );
        // A single `<` is a redirection, read as before.
        assert_eq!(sc("wc -l < f"), ex(&[&["wc", "-l", "<", "f"]]));
        // `<<` with no delimiter at the end: nothing to skip.
        assert_eq!(sc("cat <<"), ex(&[&["cat"]]));
    }

    #[test]
    fn join_keeps_plain_words_readable() {
        let w: Vec<String> = ["pnpm", "vitest", "run", "src/a.test.ts"]
            .map(String::from)
            .to_vec();
        assert_eq!(join(&w), "pnpm vitest run src/a.test.ts");
    }
}
