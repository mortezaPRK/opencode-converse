//! Resolve which participant a transcript addresses, tolerating ASR spelling variants.
//! Rust owns this: the engine owns conversation state and text shaping, and the host only
//! routes the already-resolved prompt to that participant's session.
use crate::protocol::Participant;

/// Greetings that may precede a spoken name ("hey Ada, ...").
const GREETINGS: [&str; 7] = ["hey", "hi", "hello", "ok", "okay", "yo", "so"];

#[derive(Debug, PartialEq)]
pub struct Resolved {
    pub index: usize,
    /// The transcript with the spoken name (and a greeting before it) removed.
    pub text: String,
    pub addressed: bool,
}

struct Word<'a> {
    start: usize,
    end: usize,
    text: &'a str,
}

fn words(text: &str) -> Vec<Word<'_>> {
    let mut out = vec![];
    let mut start = None;
    for (i, c) in text.char_indices() {
        let part = c.is_alphabetic() || (c == '\'' && start.is_some());
        match (part, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                out.push(Word {
                    start: s,
                    end: i,
                    text: &text[s..i],
                });
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push(Word {
            start: s,
            end: text.len(),
            text: &text[s..],
        });
    }
    out
}

fn fold(word: &str) -> Vec<char> {
    word.chars()
        .filter(|c| c.is_alphabetic())
        .flat_map(char::to_lowercase)
        .collect()
}

fn distance(a: &[char], b: &[char]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Edit distance allowed for a spoken name: short names must match exactly because one
/// edit would also turn common words into names ("Ada" vs "add").
fn tolerance(name_len: usize) -> usize {
    match name_len {
        0..=3 => 0,
        4..=5 => 1,
        _ => 2,
    }
}

fn matches(word: &str, roster: &[Participant]) -> Option<usize> {
    let word = fold(word);
    if word.is_empty() {
        return None;
    }
    let mut best: Option<(usize, usize)> = None;
    let mut tied = false;
    for (index, participant) in roster.iter().enumerate() {
        let name = fold(&participant.name);
        let d = distance(&word, &name);
        if d > tolerance(name.len()) || word.len().abs_diff(name.len()) > tolerance(name.len()) {
            continue;
        }
        match best {
            Some((_, bd)) if d > bd => {}
            Some((_, bd)) if d == bd => tied = true,
            _ => {
                best = Some((index, d));
                tied = false;
            }
        }
    }
    // An ambiguous variant is not trusted: fall back to the default addressee.
    best.filter(|_| !tied).map(|(index, _)| index)
}

fn strip_separators(text: &str) -> &str {
    text.trim_start_matches(|c: char| {
        c.is_whitespace() || matches!(c, ',' | ':' | ';' | '-' | '–' | '—' | '.' | '!' | '?')
    })
}

fn finish(index: usize, text: &str, original: &str) -> Resolved {
    let text = text.trim();
    Resolved {
        index,
        text: if text.is_empty() {
            original.trim().to_owned()
        } else {
            text.to_owned()
        },
        addressed: true,
    }
}

/// Leading ("Ada, check the build", "hey ada check") or trailing ("check the build, Ada")
/// vocative. A name in the middle of a sentence is not addressing ("ask Grace to ...").
/// With one participant nothing is stripped. No name resolves to the first participant.
pub fn resolve(roster: &[Participant], transcript: &str) -> Resolved {
    let default = Resolved {
        index: 0,
        text: transcript.to_owned(),
        addressed: false,
    };
    if roster.len() < 2 {
        return default;
    }
    let ws = words(transcript);
    let mut at = 0;
    if ws.len() > 1 && GREETINGS.contains(&fold(ws[0].text).iter().collect::<String>().as_str()) {
        at = 1;
    }
    if let Some(word) = ws.get(at) {
        if let Some(index) = matches(word.text, roster) {
            return finish(index, strip_separators(&transcript[word.end..]), transcript);
        }
    }
    if let Some(last) = ws.last() {
        if let Some(index) = matches(last.text, roster) {
            let before = transcript[..last.start].trim_end();
            let after = transcript[last.end..].trim();
            let vocative = before.is_empty()
                || before.ends_with(',')
                || before.ends_with('-')
                || before.ends_with('–');
            if vocative && after.chars().all(|c| matches!(c, '.' | '!' | '?' | ',')) {
                let before = before.trim_end_matches([',', '-', '–', ' ']);
                return finish(index, before, transcript);
            }
        }
    }
    default
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roster() -> Vec<Participant> {
        ["Ada", "Grace", "Linus"]
            .iter()
            .enumerate()
            .map(|(i, name)| Participant {
                id: format!("a{}", i + 1),
                name: (*name).into(),
                voice: "default".into(),
            })
            .collect()
    }

    fn r(text: &str) -> (usize, String, bool) {
        let r = resolve(&roster(), text);
        (r.index, r.text, r.addressed)
    }

    #[test]
    fn leading_name_is_resolved_and_stripped() {
        assert_eq!(
            r("Grace, check the build."),
            (1, "check the build.".into(), true)
        );
        assert_eq!(
            r("grace check the build"),
            (1, "check the build".into(), true)
        );
        assert_eq!(
            r("Hey Linus: what failed?"),
            (2, "what failed?".into(), true)
        );
        assert_eq!(
            r("Okay, Ada. Run the tests"),
            (0, "Run the tests".into(), true)
        );
    }

    #[test]
    fn trailing_name_needs_a_vocative_comma() {
        assert_eq!(
            r("Check the build, Grace."),
            (1, "Check the build".into(), true)
        );
        assert_eq!(
            r("check the build Grace"),
            (0, "check the build Grace".into(), false)
        );
    }

    #[test]
    fn mid_sentence_names_do_not_address() {
        assert_eq!(
            r("ask Grace to check it"),
            (0, "ask Grace to check it".into(), false)
        );
    }

    #[test]
    fn asr_variants_resolve_and_ambiguous_ones_do_not() {
        assert_eq!(r("Gracie, what now").0, 1);
        assert_eq!(r("Grase, what now").0, 1);
        assert_eq!(r("Linnus check").0, 2);
        assert_eq!(r("Lines check").0, 2);
        // Short names must be exact: "add" is not Ada.
        assert!(!r("add the test").2);
        assert_eq!(r("Ada, add the test"), (0, "add the test".into(), true));
    }

    #[test]
    fn no_name_resolves_to_the_first_agent_unchanged() {
        assert_eq!(
            r("What is the status?"),
            (0, "What is the status?".into(), false)
        );
    }

    #[test]
    fn name_only_keeps_the_original_text() {
        assert_eq!(r("Grace."), (1, "Grace.".into(), true));
    }

    #[test]
    fn a_single_participant_never_strips() {
        let one = vec![roster().remove(0)];
        let r = resolve(&one, "Ada, hello");
        assert_eq!(
            (r.index, r.text.as_str(), r.addressed),
            (0, "Ada, hello", false)
        );
    }

    #[test]
    fn two_names_within_tolerance_are_ambiguous() {
        let roster: Vec<Participant> = ["Mark", "Marc"]
            .iter()
            .enumerate()
            .map(|(i, name)| Participant {
                id: format!("a{}", i + 1),
                name: (*name).into(),
                voice: "default".into(),
            })
            .collect();
        assert!(!resolve(&roster, "Marq, hello").addressed);
        assert_eq!(resolve(&roster, "Marc, hello").index, 1);
    }

    #[test]
    fn unicode_and_empty_input_are_safe() {
        assert!(!r("").2);
        assert!(!r("Ünï, 🦀").2);
        assert_eq!(r("Grace,").0, 1);
    }
}
