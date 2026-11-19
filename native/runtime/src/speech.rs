//! One incremental cleanup/chunking stage, shared by native and explicit test mode.
//! Markdown/code is not read verbatim. State survives arbitrary delta boundaries.

// Keep complete sentences when possible, without allowing unbounded synthesis.
const MAX_CHUNK_BYTES: usize = 320;
const MAX_CHUNK_WORDS: usize = 40;

pub struct Speech {
    raw: String,
    pending: String,
    fence: Option<(char, usize)>,
    inline: Option<usize>,
    line_start: bool,
    quote_prefix: bool,
}

impl Default for Speech {
    fn default() -> Self {
        Self {
            raw: String::new(),
            pending: String::new(),
            fence: None,
            inline: None,
            line_start: true,
            quote_prefix: true,
        }
    }
}

impl Speech {
    pub fn push(&mut self, text: &str, final_: bool) -> Vec<String> {
        self.raw.push_str(text);
        // Process complete lines; this makes fenced-code handling independent of delta splits.
        let mut raw = std::mem::take(&mut self.raw);
        complete_lines(&mut raw, |line| self.line(line));
        self.raw = raw;
        if final_ && !self.raw.is_empty() {
            let line = std::mem::take(&mut self.raw);
            self.line(&line);
        } else if self.fence.is_none()
            && !(self.line_start && self.inline.is_none() && could_open_fence(&self.raw))
        {
            let raw = std::mem::take(&mut self.raw);
            let consumed = self.clean(&raw, false);
            self.raw.push_str(&raw[consumed..]);
            if consumed > 0 {
                self.line_start = false;
            }
        }
        self.chunks(final_)
    }

    fn line(&mut self, line: &str) {
        let trimmed = line.trim_start();
        let delimiter = trimmed.chars().next().filter(|c| matches!(c, '`' | '~'));
        let run = delimiter.map_or(0, |c| trimmed.chars().take_while(|x| *x == c).count());
        if self.line_start && run >= 3 {
            if let Some((opening, length)) = self.fence {
                if delimiter == Some(opening) && run >= length && trimmed[run..].trim().is_empty() {
                    self.fence = None;
                }
                self.line_start = true;
                self.quote_prefix = true;
                return;
            } else if self.inline.is_none()
                && !(delimiter == Some('`') && trimmed[run..].contains('`'))
            {
                self.fence = Some((delimiter.unwrap(), run));
                self.line_start = true;
                self.quote_prefix = true;
                return;
            }
        }
        if self.fence.is_none() {
            self.clean(line, true);
        }
        self.line_start = true;
        self.quote_prefix = true;
    }

    /// Consume only complete delimiters/links/tags; ordinary prose can always
    /// advance without whitespace, including at arbitrary UTF-8 delta boundaries.
    fn clean(&mut self, line: &str, complete: bool) -> usize {
        let mut pos = 0;
        while pos < line.len() {
            let rest = &line[pos..];
            let c = rest.chars().next().unwrap();
            if c == '`' {
                let run = rest.bytes().take_while(|b| *b == b'`').count();
                if run == rest.len() && !complete {
                    break;
                }
                if self.inline == Some(run) {
                    self.inline = None;
                } else if self.inline.is_none() {
                    self.inline = Some(run);
                }
                pos += run;
                self.quote_prefix = false;
                continue;
            }
            if self.inline.is_some() {
                pos += c.len_utf8();
                self.quote_prefix = false;
                continue;
            }
            if c == '[' {
                if let Some(end) = rest.find(']') {
                    let mut consumed = end + 1;
                    if consumed == rest.len() && !complete {
                        break;
                    }
                    if rest[consumed..].starts_with('(') {
                        if let Some(close) = link_end(&rest[consumed..]) {
                            consumed += close;
                        } else if !complete {
                            break;
                        } else {
                            consumed = rest.len();
                        }
                    }
                    // Labels use inline cleanup, not block parsing, and cannot
                    // leave their delimiter state in the surrounding prose.
                    let mut label = Speech {
                        quote_prefix: false,
                        ..Speech::default()
                    };
                    label.clean(&rest[1..end], true);
                    self.pending.push_str(&label.pending);
                    pos += consumed;
                    self.quote_prefix = false;
                    continue;
                }
                if !complete {
                    break;
                }
            }
            if c == '<' {
                if rest.len() == 1 && !complete {
                    break;
                }
                if let Some(end) = rest.find('>') {
                    let inside = &rest[1..end];
                    let tag = inside.trim_start_matches('/');
                    if tag.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                        || inside.starts_with('!')
                        || inside.starts_with('?')
                    {
                        pos += end + 1;
                        self.quote_prefix = false;
                        continue;
                    }
                } else if !complete
                    && rest[1..]
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_alphabetic() || matches!(c, '/' | '!' | '?'))
                {
                    break;
                }
            }
            let quote_marker = c == '>' && self.quote_prefix;
            if !c.is_whitespace() && !quote_marker {
                self.quote_prefix = false;
            }
            pos += c.len_utf8();
            if matches!(c, '*' | '#' | '_' | '|') || quote_marker {
                continue;
            }
            if c.is_control() && !c.is_whitespace() {
                continue;
            }
            if c.is_whitespace() {
                if !self.pending.ends_with(' ') {
                    self.pending.push(' ');
                }
            } else {
                self.pending.push(c);
            }
        }
        pos
    }

    fn chunks(&mut self, final_: bool) -> Vec<String> {
        let mut chunks = Vec::new();
        self.chunks_with(final_, |text| chunks.push(text.to_owned()));
        chunks
    }

    fn chunks_with(&mut self, final_: bool, mut emit: impl FnMut(&str)) {
        let mut processed = 0;
        loop {
            let pending = &self.pending[processed..];
            // Decide forced cuts from a fixed prefix, not the size of a provider
            // delta. Otherwise batch and streamed replies acquire different prosody.
            let mut words = 0;
            let mut in_word = false;
            let word_bound = pending
                .char_indices()
                .take_while(|(i, _)| *i <= MAX_CHUNK_BYTES)
                .find_map(|(i, c)| {
                    if c.is_whitespace() {
                        if in_word {
                            words += 1;
                            in_word = false;
                        }
                        (words == MAX_CHUNK_WORDS).then_some(i)
                    } else {
                        in_word = true;
                        None
                    }
                });
            let horizon = word_bound.unwrap_or(MAX_CHUNK_BYTES);
            let punctuation = pending
                .char_indices()
                .take_while(|(i, c)| i + c.len_utf8() <= horizon)
                .find_map(|(i, c)| {
                    if matches!(c, '。' | '！' | '？') {
                        Some(i + c.len_utf8())
                    } else if matches!(c, '.' | '!' | '?')
                        && (c != '.' || !continuation_period(pending, i))
                    {
                        ascii_sentence_end(pending, i + c.len_utf8(), final_)
                            .filter(|end| *end <= horizon)
                    } else {
                        None
                    }
                });
            let forced = word_bound.is_some() || pending.len() > MAX_CHUNK_BYTES;
            let cut = punctuation.or_else(|| {
                if !forced {
                    return None;
                }
                // Prefer a complete clause over an arbitrary word splice. Wait
                // until the safety horizon before selecting it, so commas in short
                // sentences keep their model-generated internal timing.
                let clause = pending
                    .char_indices()
                    .take_while(|(i, c)| i + c.len_utf8() <= horizon)
                    .filter(|(_, c)| matches!(c, ',' | ';' | ':' | '—' | '–'))
                    .filter_map(|(i, c)| {
                        let end = i + c.len_utf8();
                        pending[end..]
                            .chars()
                            .next()
                            .is_some_and(char::is_whitespace)
                            .then_some(end)
                    })
                    .last();
                clause.or(word_bound).or_else(|| {
                    pending
                        .char_indices()
                        .take_while(|(i, _)| *i <= horizon)
                        .filter(|(i, c)| *i > 0 && c.is_whitespace())
                        .map(|(i, _)| i)
                        .last()
                        .or_else(|| {
                            // Unbroken tokens still need a positive UTF-8-safe cut.
                            pending
                                .char_indices()
                                .map(|(i, c)| i + c.len_utf8())
                                .take_while(|end| *end <= horizon)
                                .last()
                        })
                })
            });
            let Some(cut) = cut else {
                break;
            };
            let text = pending[..cut].trim();
            if !text.is_empty() {
                emit(text);
            }
            processed += cut;
        }
        if final_ {
            let text = self.pending[processed..].trim();
            if !text.is_empty() {
                emit(text);
            }
            processed = self.pending.len();
        }
        if processed > 0 {
            self.pending.drain(..processed);
        }
    }
}

fn complete_lines(raw: &mut String, mut line: impl FnMut(&str)) {
    let mut processed = 0;
    while let Some(end) = raw[processed..].find('\n') {
        let end = processed + end + 1;
        line(&raw[processed..end]);
        processed = end;
    }
    if processed > 0 {
        raw.drain(..processed);
    }
}

/// Conservative English continuation forms. Only inspect the prefix: a decision
/// must not change when another provider delta arrives after the whitespace.
fn continuation_period(text: &str, at: usize) -> bool {
    let token = text[..at]
        .split_whitespace()
        .next_back()
        .unwrap_or("")
        .trim_start_matches(['(', '[', '{', '"', '\'', '“', '‘']);
    let lower = token.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "mr" | "mrs" | "ms" | "dr" | "prof" | "sr" | "jr" | "st" | "vs" | "e.g" | "i.e"
    ) {
        return true;
    }
    // Initials and dotted acronyms (J. Smith, U.S. team), not decimal/version
    // numbers. Ambiguous acronyms at sentence end remain together until final
    // flush or a later unambiguous boundary.
    token
        .split('.')
        .all(|part| part.len() == 1 && part.as_bytes()[0].is_ascii_uppercase())
}

fn ascii_sentence_end(text: &str, mut end: usize, final_: bool) -> Option<usize> {
    while let Some(c) = text[end..].chars().next() {
        if matches!(c, ')' | ']' | '}' | '"' | '\'' | '”' | '’') {
            end += c.len_utf8();
        } else {
            return c.is_whitespace().then_some(end);
        }
    }
    final_.then_some(end)
}

fn could_open_fence(text: &str) -> bool {
    let text = text.trim_start();
    let Some(delimiter) = text.chars().next() else {
        return true;
    };
    if !matches!(delimiter, '`' | '~') {
        return false;
    }
    let run = text
        .bytes()
        .take_while(|byte| *byte == delimiter as u8)
        .count();
    run == text.len() || (run >= 3 && (delimiter == '~' || !text[run..].contains('`')))
}

fn link_end(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut escaped = false;
    for (at, c) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
            continue;
        }
        if c == '(' {
            depth += 1;
        }
        if c == ')' {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(at + 1);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_chunk_scan_borrows_offsets_and_keeps_the_unfinished_suffix() {
        let mut speech = Speech::default();
        speech.pending = format!("{}The value is 3.", "界。".repeat(30_000));
        assert!(speech.pending.len() <= crate::protocol::MAX_TEXT_BYTES);
        let base = speech.pending.as_ptr() as usize;
        let mut count = 0;
        speech.chunks_with(false, |text| {
            assert_eq!(text, "界。");
            assert_eq!(
                text.as_ptr() as usize,
                base + count * 6,
                "chunk copied/shifted instead of scanning original offsets"
            );
            count += 1;
        });
        assert_eq!(count, 30_000);
        assert_eq!(speech.pending, "The value is 3.");
        assert_eq!(speech.chunks(true), ["The value is 3."]);
    }
    #[test]
    fn complete_line_scan_uses_offset_slices_and_keeps_a_partial_utf8_suffix() {
        let mut raw = format!("{}partial界", "é\n".repeat(20_000));
        let base = raw.as_ptr() as usize;
        let mut processed = 0;
        let mut lines = 0;
        complete_lines(&mut raw, |line| {
            assert_eq!(
                line.as_ptr() as usize,
                base + processed,
                "line allocated/copied instead of scanning the original prefix"
            );
            assert_eq!(line, "é\n");
            processed += line.len();
            lines += 1;
        });
        assert_eq!(lines, 20_000);
        assert_eq!(processed, 60_000);
        assert_eq!(raw, "partial界");
    }
    #[test]
    fn literal_comparisons_and_logical_blockquotes_survive_every_split() {
        for (text, expected) in [
            (
                "The value must be > 10, not = 10.",
                "The value must be > 10, not = 10.",
            ),
            (
                "> Quoted speech stays clean.\n",
                "Quoted speech stays clean.",
            ),
            (
                "  > > Quoted speech stays clean.\nThe value must be > 10.",
                "Quoted speech stays clean. The value must be > 10.",
            ),
        ] {
            for split in 0..=text.len() {
                let mut speech = Speech::default();
                let mut out = speech.push(&text[..split], false);
                out.extend(speech.push(&text[split..], true));
                assert_eq!(out.join(" "), expected, "split {split}: {text}");
            }
        }
    }
    #[test]
    fn decimal_and_version_chunks_are_stable_at_every_delta_split() {
        for (text, expected) in [
            ("The value is 3.14.", vec!["The value is 3.14."]),
            ("The value is -3.14.", vec!["The value is -3.14."]),
            ("The version is 1.2.3.", vec!["The version is 1.2.3."]),
            (
                "Pi is about 3.14159. Continue with that value.",
                vec!["Pi is about 3.14159.", "Continue with that value."],
            ),
            (
                "The value is 3.14.) Next value is -2.5.",
                vec!["The value is 3.14.)", "Next value is -2.5."],
            ),
            (
                "The version is 1.2.3.\" Next value is 3.14.",
                vec!["The version is 1.2.3.\"", "Next value is 3.14."],
            ),
        ] {
            assert_eq!(Speech::default().push(text, true), expected);
            for split in 0..=text.len() {
                let mut speech = Speech::default();
                let mut chunks = speech.push(&text[..split], false);
                chunks.extend(speech.push(&text[split..], false));
                chunks.extend(speech.push("", true));
                assert_eq!(chunks, expected, "split {split}: {text}");
            }
        }
    }
    #[test]
    fn closing_punctuation_does_not_establish_a_boundary_without_lookahead() {
        let mut speech = Speech::default();
        assert!(speech.push("This is ready.)", false).is_empty());
        assert_eq!(speech.push(" ", false), ["This is ready.)"]);
        let mut speech = Speech::default();
        assert!(speech.push("This is ready.)", false).is_empty());
        assert!(speech.push("suffix", false).is_empty());
        assert_eq!(speech.push(".", true), ["This is ready.)suffix."]);
    }
    #[test]
    fn trailing_ascii_punctuation_waits_for_a_real_boundary() {
        let mut speech = Speech::default();
        assert!(speech.push("The value is 3.", false).is_empty());
        assert!(speech.push("14.", false).is_empty());
        assert_eq!(speech.push("", true), ["The value is 3.14."]);
        for punctuation in ['.', '!', '?'] {
            let mut speech = Speech::default();
            let sentence = format!("This is ready{punctuation}");
            assert!(speech.push(&sentence, false).is_empty());
            assert_eq!(speech.push(" ", false), [sentence]);
        }
    }
    #[test]
    fn completed_leading_inline_code_does_not_stall_ordinary_prose() {
        for code in ["`secret()` ", "``secret()`` ", "```secret()``` "] {
            for split in 0..=code.len() {
                let mut speech = Speech::default();
                let mut out = speech.push(&code[..split], false);
                out.extend(speech.push(&code[split..], false));
                for _ in 0..40 {
                    out.extend(speech.push("This is ordinary spoken prose. ", false));
                }
                assert_eq!(out.len(), 40, "split {split}: {code}");
                assert!(out
                    .iter()
                    .all(|text| text == "This is ordinary spoken prose."));
            }
        }
    }
    #[test]
    fn invalid_fence_info_and_link_cleanup_survive_every_split() {
        for (text, expected) in [
            (
                "```hidden``` followed by spoken prose.\n",
                "followed by spoken prose.",
            ),
            (
                "Read [guide](https://example.test/a_(b)/suffix).",
                "Read guide.",
            ),
            (r"Read [guide](https://example.test/a\)b).", "Read guide."),
            ("[Read `secret()`](https://example.com) now.", "Read now."),
            (
                "[**Read** `unclosed](https://example.com) now.",
                "Read now.",
            ),
        ] {
            for split in 0..=text.len() {
                let mut speech = Speech::default();
                let mut out = speech.push(&text[..split], false);
                out.extend(speech.push(&text[split..], true));
                assert_eq!(out.join(" "), expected, "split {split}: {text}");
            }
        }
    }
    #[test]
    fn cjk_streams_before_finalization() {
        let mut speech = Speech::default();
        assert!(!speech
            .push(&"これは長い文章です。".repeat(50), false)
            .is_empty());
    }
    #[test]
    fn streamed_prefix_preserves_logical_line_start() {
        let mut speech = Speech::default();
        let mut out = speech.push("Use ", false);
        out.extend(speech.push("```some code``` in this sentence.\n", true));
        assert_eq!(out.join(" "), "Use in this sentence.");
    }
    #[test]
    fn fences_close_only_with_matching_delimiter() {
        let text = "Before.\n````rust\nsecret\n```\nstill secret\n~~~~\nsecret again\n```` trailing\nsecret too\n````\nAfter.\n";
        for split in 0..=text.len() {
            let mut speech = Speech::default();
            let mut out = speech.push(&text[..split], false);
            out.extend(speech.push(&text[split..], true));
            assert_eq!(out.join(" "), "Before. After.", "split {split}");
        }
    }
    #[test]
    fn inline_runs_and_comparisons_survive_every_delta_split() {
        for text in [
            "Use ``a ` literal`` safely now.",
            "The value is < 10, so continue.",
            "Read <b>the guide</b> safely now.",
        ] {
            let expected = match text {
                "Use ``a ` literal`` safely now." => "Use safely now.",
                "Read <b>the guide</b> safely now." => "Read the guide safely now.",
                _ => text,
            };
            for split in 0..=text.len() {
                let mut speech = Speech::default();
                let mut out = speech.push(&text[..split], false);
                out.extend(speech.push(&text[split..], true));
                assert_eq!(out.join(" "), expected, "split {split}: {text}");
            }
        }
    }
    #[test]
    fn cleanup_across_deltas() {
        let mut speech = Speech::default();
        let mut out = vec![];
        for text in [
            "Hello **wor",
            "ld**.\n`",
            "``rust\nsecret();\n```\n",
            "Read [the guide](https://example.com).",
        ] {
            out.extend(speech.push(text, false));
        }
        out.extend(speech.push("", true));
        assert_eq!(out.join(" "), "Hello world. Read the guide.");
    }
    #[test]
    fn complete_sentences_survive_the_old_word_cap_at_every_split() {
        let text = format!("{}finished. Next sentence.", "word ".repeat(30));
        let expected = vec![
            format!("{}finished.", "word ".repeat(30)),
            "Next sentence.".into(),
        ];
        for split in 0..=text.len() {
            let mut speech = Speech::default();
            let mut chunks = speech.push(&text[..split], false);
            chunks.extend(speech.push(&text[split..], false));
            chunks.extend(speech.push("", true));
            assert_eq!(chunks, expected, "split {split}");
        }
    }
    #[test]
    fn forced_chunks_prefer_clauses_independently_of_delta_boundaries() {
        for separator in [",", ";", ":", "—", "–"] {
            let first = format!("{}ready{separator}", "word ".repeat(18));
            let text = format!("{first} {}finished.", "word ".repeat(35));
            let expected = vec![first, format!("{}finished.", "word ".repeat(35))];
            for split in text.char_indices().map(|(i, _)| i).chain([text.len()]) {
                let mut speech = Speech::default();
                let mut chunks = speech.push(&text[..split], false);
                chunks.extend(speech.push(&text[split..], false));
                chunks.extend(speech.push("", true));
                assert_eq!(chunks, expected, "split {split}: {separator}");
            }
            let mut speech = Speech::default();
            let mut chunks = Vec::new();
            for c in text.chars() {
                chunks.extend(speech.push(&c.to_string(), false));
            }
            chunks.extend(speech.push("", true));
            assert_eq!(chunks, expected);
        }
    }
    #[test]
    fn byte_horizon_prefers_clauses_and_word_horizon_still_bounds_short_words() {
        let first = format!("{}ready,", "lengthyword ".repeat(12));
        let text = format!("{first} {}done.", "lengthyword ".repeat(20));
        let chunks = Speech::default().push(&text, true);
        assert_eq!(chunks[0], first);
        assert!(chunks.iter().all(|chunk| chunk.len() <= MAX_CHUNK_BYTES));
        let chunks = Speech::default().push(&"a ".repeat(100), true);
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.split_whitespace().count())
                .collect::<Vec<_>>(),
            [40, 40, 20]
        );
    }
    #[test]
    fn safety_horizon_cuts_preserve_words_at_every_delta_split() {
        for text in [
            format!("{}tail.", "lengthyword ".repeat(60)),
            format!("{}tail.", "a ".repeat(100)),
            format!(
                "{}boundary, {}tail.",
                "word ".repeat(60),
                "word ".repeat(50)
            ),
        ] {
            let expected = Speech::default().push(&text, true);
            for split in 0..=text.len() {
                let mut speech = Speech::default();
                let mut chunks = speech.push(&text[..split], false);
                chunks.extend(speech.push(&text[split..], false));
                chunks.extend(speech.push("", true));
                assert_eq!(chunks, expected, "split {split}");
                assert_eq!(chunks.join(" "), text);
            }
        }
    }
    #[test]
    fn abbreviations_initials_and_short_sentences_are_stable_at_every_split() {
        for text in [
            "Please ask Mr. Smith to continue. Done.",
            "Please ask Dr. Jones to continue. Done.",
            "Ask Prof. J. Smith about the U.S. team. Done.",
            "Try another option, e.g. a smaller model. Done.",
            "Use natural phrasing, i.e. complete sentences. Done.",
            "Compare the small model vs. the large model. Done.",
        ] {
            let expected = vec![text.strip_suffix(" Done.").unwrap(), "Done."];
            for split in 0..=text.len() {
                let mut speech = Speech::default();
                let mut chunks = speech.push(&text[..split], false);
                chunks.extend(speech.push(&text[split..], false));
                chunks.extend(speech.push("", true));
                assert_eq!(chunks, expected, "split {split}: {text}");
            }
        }
        assert_eq!(
            Speech::default().push("Sure. Ready? Yes!", true),
            ["Sure.", "Ready?", "Yes!"]
        );
        let mut speech = Speech::default();
        assert!(speech.push("Sure.", false).is_empty());
        assert_eq!(speech.push(" ", false), ["Sure."]);
    }
    #[test]
    fn bounded_chunks() {
        let chunks = Speech::default().push(&"word ".repeat(10_000), true);
        assert!(chunks.iter().all(|c| c.len() <= 320));
    }
    #[test]
    fn sentence_followed_by_unbroken_unicode_always_makes_progress() {
        for prefix in ["A sentence. ", "This is a sentence. ", " "] {
            let text = format!("{prefix}{}", "界🦀é".repeat(2000));
            let chunks = Speech::default().push(&text, true);
            assert!(!chunks.is_empty());
            assert!(chunks
                .iter()
                .all(|chunk| !chunk.is_empty() && chunk.len() <= 320));
            let compact = |text: &str| {
                text.chars()
                    .filter(|c| !c.is_whitespace())
                    .collect::<String>()
            };
            assert_eq!(compact(&chunks.concat()), compact(&text));
        }
    }
    #[test]
    fn no_whitespace_unicode_uses_bounded_positive_utf8_cuts() {
        for token in ["界", "🦀", "é", "界🦀é", "."] {
            let text = token.repeat(3000);
            let mut speech = Speech::default();
            let mut chunks = speech.push(&text, false);
            chunks.extend(speech.push("", true));
            assert!(chunks.len() > 1);
            assert!(chunks
                .iter()
                .all(|chunk| !chunk.is_empty() && chunk.len() <= 320));
            assert_eq!(chunks.concat(), text);
        }
    }
}
