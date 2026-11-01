//! The model input, built from what the user would see: the text on the screen, the terminal title, the working
//! directory and branch, the foreground program and (when the shell hooks run) the last command's outcome, plus the
//! previous summary for a rolling update. The same input for every kind of session: a plain shell, a shell inside
//! `docker exec` or `ssh` (which the hooks cannot see into), an editor or an AI agent's TUI.

/// Screen lines given to the model (the bottom ones: the newest output and the prompt).
pub const SCREEN_LINES: usize = 30;
const SCREEN_HEAD: &str = "screen (newest at the bottom):\n";
/// Longest screen line given to the model.
pub const LINE_CHARS: usize = 120;

/// Box drawing, block and frame glyphs that TUIs draw borders and bars with.
fn is_frame(c: char) -> bool {
    matches!(c, '\u{2500}'..='\u{259f}' | '\u{2800}' | '|' | '+' | '-' | '=' | '_' | '~' | '·' | '•')
}

/// The screen text the model sees: rows as the user sees them, right-trimmed and clipped to `LINE_CHARS`, borders of
/// full-screen programs stripped (a row that is only frame glyphs counts as blank), runs of blank rows collapsed to
/// one, leading and trailing blank rows dropped, at most the bottom `SCREEN_LINES`.
pub fn screen_lines(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim_end();
        let line = if line.trim_start().starts_with(|c: char| ('\u{2500}'..='\u{259f}').contains(&c)) {
            // `│ text │` inside a box: keep the text.
            line.trim_matches(|c: char| c.is_whitespace() || ('\u{2500}'..='\u{259f}').contains(&c))
        } else {
            line
        };
        let blank = line.chars().all(|c| c.is_whitespace() || is_frame(c));
        if blank {
            if out.last().is_some_and(|l| !l.is_empty()) {
                out.push(String::new());
            }
            continue;
        }
        let clipped: String = line.chars().take(LINE_CHARS).collect();
        out.push(if line.chars().count() > LINE_CHARS { format!("{clipped}…") } else { clipped });
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    let skip = out.len().saturating_sub(SCREEN_LINES);
    out.drain(..skip);
    while out.first().is_some_and(|l| l.is_empty()) {
        out.remove(0);
    }
    out
}

/// Everything the digest is made of.
#[derive(Debug, Default)]
pub struct Input<'a> {
    /// The previous summary (live-state phrases already dropped): this run is a rolling update. Goes last.
    pub earlier: Option<&'a str>,
    /// `cwd: ~/src/x (git x, branch main)`, possibly followed by a `branch changed …` line.
    pub header: &'a str,
    /// The meaningful terminal title (empty: none).
    pub title: &'a str,
    /// The foreground program's command line (empty: unknown).
    pub foreground: &'a str,
    /// From the shell hooks: `last command: make · exit 2` or `running command: make` (empty: no hooks).
    pub last_command: &'a str,
    /// The screen's text (`vt100::Screen::contents`).
    pub screen: &'a str,
}

/// The digest: a few labelled lines, then the screen rows prefixed with `| ` (so the model cannot confuse screen text
/// with the labels).
pub fn digest(i: &Input) -> String {
    let mut out = String::new();
    out.push_str(i.header.trim_end());
    out.push('\n');
    if !i.title.is_empty() {
        out.push_str(&format!("title: \"{}\"\n", i.title));
    }
    if !i.foreground.is_empty() {
        out.push_str(&format!("foreground: {}\n", i.foreground));
    }
    if !i.last_command.is_empty() {
        out.push_str(i.last_command);
        out.push('\n');
    }
    let lines = screen_lines(i.screen);
    if lines.is_empty() {
        out.push_str("screen: (empty)\n");
    } else {
        out.push_str(SCREEN_HEAD);
        for l in lines {
            out.push_str(if l.is_empty() { "|" } else { "| " });
            out.push_str(&l);
            out.push('\n');
        }
    }
    // After the screen, so a small model reads the newest state first and treats this as background.
    if let Some(e) = i.earlier {
        out.push_str(&format!("older history (before this screen; may be out of date): {}\n", e.trim()));
    }
    out
}

/// The digest without its oldest (top) screen row, for a digest too long for the model; None when no row is left.
pub fn drop_oldest_row(digest: &str) -> Option<String> {
    let start = digest.find(SCREEN_HEAD)? + SCREEN_HEAD.len();
    let row = &digest[start..];
    if !row.starts_with('|') {
        return None;
    }
    let end = start + row.find('\n').map_or(row.len(), |i| i + 1);
    let rest = &digest[end..];
    if rest.starts_with('|') {
        // A blank row is not worth keeping at the top.
        let next = if rest.starts_with("|\n") { 2 } else { 0 };
        return Some(format!("{}{}", &digest[..start], &rest[next..]));
    }
    // That was the last row.
    Some(format!("{}screen: (empty)\n{}", &digest[..digest.find(SCREEN_HEAD)?], rest))
}

/// The digest with only the bottom half of its screen rows (at least 4 must be there); None if there are fewer.
pub fn keep_bottom_half(digest: &str) -> Option<String> {
    let start = digest.find(SCREEN_HEAD)? + SCREEN_HEAD.len();
    let rows = digest[start..].lines().take_while(|l| l.starts_with('|')).count();
    if rows < 4 {
        return None;
    }
    let mut d = digest.to_string();
    for _ in 0..rows / 2 {
        d = drop_oldest_row(&d)?;
    }
    Some(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bottom_half_of_the_screen() {
        let d = "cwd: ~\nscreen (newest at the bottom):\n| 1\n| 2\n| 3\n| 4\n| 5\nolder history (before this screen; may be out of date): x\n";
        assert_eq!(keep_bottom_half(d).unwrap(), "cwd: ~\nscreen (newest at the bottom):\n| 3\n| 4\n| 5\nolder history (before this screen; may be out of date): x\n");
        assert_eq!(keep_bottom_half("cwd: ~\nscreen (newest at the bottom):\n| 1\n| 2\n| 3\n"), None);
    }

    #[test]
    fn dropping_screen_rows_keeps_the_rest() {
        let d = "cwd: ~\nscreen (newest at the bottom):\n| one\n|\n| two\nolder history (before this screen; may be out of date): x\n";
        let d1 = drop_oldest_row(d).unwrap();
        assert_eq!(d1, "cwd: ~\nscreen (newest at the bottom):\n| two\nolder history (before this screen; may be out of date): x\n");
        let d2 = drop_oldest_row(&d1).unwrap();
        assert_eq!(d2, "cwd: ~\nscreen: (empty)\nolder history (before this screen; may be out of date): x\n");
        assert_eq!(drop_oldest_row(&d2), None);
    }

    #[test]
    fn screen_text_is_cleaned() {
        let text = "\n\n$ make\ncc -c a.c\n\n\n\na.c:3: error: x\n╭──────────╮\n│ > hello  │\n╰──────────╯\n────────────\n$   \n\n";
        assert_eq!(screen_lines(text), vec!["$ make", "cc -c a.c", "", "a.c:3: error: x", "", "> hello", "", "$"]);
        let long = "x".repeat(200);
        let l = screen_lines(&long);
        assert_eq!(l[0].chars().count(), LINE_CHARS + 1);
        let many: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let l = screen_lines(&many);
        assert_eq!((l.len(), l[0].as_str(), l[SCREEN_LINES - 1].as_str()), (SCREEN_LINES, "line 70", "line 99"));
        assert!(screen_lines("\n \n───\n").is_empty());
    }

    #[test]
    fn digest_format() {
        let d = digest(&Input {
            earlier: Some("Ran cargo build; it failed. "),
            header: "cwd: ~/src/app (git app, branch main)",
            title: "cargo build",
            foreground: "bash",
            last_command: "last command: cargo build · ok",
            screen: "$ cargo build\n    Finished `dev` profile\n$ ",
        });
        assert_eq!(
            d,
            "cwd: ~/src/app (git app, branch main)\ntitle: \"cargo build\"\nforeground: bash\nlast command: cargo build · ok\nscreen (newest at the bottom):\n| $ cargo build\n|     Finished `dev` profile\n| $\nolder history (before this screen; may be out of date): Ran cargo build; it failed.\n"
        );
        let d = digest(&Input { header: "cwd: ~", ..Default::default() });
        assert_eq!(d, "cwd: ~\nscreen: (empty)\n");
    }
}
