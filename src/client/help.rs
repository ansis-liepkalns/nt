//! `C-] ?`: a small centered overlay listing the prefix keys and the main switcher keys.

use super::switcher::fit;

/// Display name of the prefix byte (`0x1d` -> `C-]`).
pub fn prefix_name(b: u8) -> String {
    if b < 0x20 {
        let c = (b + 0x40) as char;
        format!("C-{}", c.to_ascii_lowercase())
    } else {
        format!("{}", b as char)
    }
}

fn lines(p: &str) -> Vec<String> {
    let keys = [
        (format!("{p} {p}"), "sessions"),
        (format!("{p} d"), "detach"),
        (format!("{p} n / p"), "next / previous session"),
        (format!("{p} l"), "last session"),
        (format!("{p} c"), "new session"),
        (format!("{p} r"), "rename"),
        (format!("{p} k"), "close session"),
        (format!("{p} q"), "quit client"),
        (format!("{p} ]"), "send a literal prefix"),
    ];
    let kw = keys.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0);
    let mut v: Vec<String> = keys.iter().map(|(k, d)| format!("{k:<kw$}  {d}")).collect();
    v.push("In the list:".into());
    v.push("RET attach · C-o new · C-k close".into());
    v.push("M-r rename · C-s search · M-i info".into());
    v.push("M-s sort · C-g cancel".into());
    v
}

/// Escape sequences drawing the box centered on a `cols` x `rows` screen. Long content is truncated (the
/// last content row then says so); every row fits the box, and the box fits the screen.
pub fn render(cols: u16, rows: u16, prefix: u8, ascii: bool) -> String {
    let (cols, rows) = (cols as usize, rows as usize);
    if cols < 6 || rows < 3 {
        return String::new();
    }
    let p = prefix_name(prefix);
    let mut body = lines(&p);
    let width = body.iter().map(|l| l.chars().count()).max().unwrap_or(0).max(" any key closes ".len() + 2) + 4;
    let w = width.min(cols);
    let inner = w - 2;
    let max_body = rows - 2;
    let footer = "any key closes";
    // Keep one row for the footer when the body does not fit.
    if body.len() + 1 > max_body {
        body.truncate(max_body.saturating_sub(1));
        if let Some(last) = body.last_mut() {
            *last = fit(&format!("{}…", last.trim_end().chars().take(inner.saturating_sub(2)).collect::<String>()), inner);
        }
    }
    let (tl, tr, bl, br, h, v) = if ascii { ('+', '+', '+', '+', '-', '|') } else { ('┌', '┐', '└', '┘', '─', '│') };
    let mut content: Vec<String> = body;
    if max_body > content.len() {
        content.push(footer.to_string());
    }
    let h_total = content.len() + 2;
    let top = (rows - h_total) / 2 + 1;
    let left = (cols - w) / 2 + 1;
    let t = format!("{h} keys ");
    let title = if inner >= t.chars().count() { format!("{t}{}", h.to_string().repeat(inner - t.chars().count())) } else { h.to_string().repeat(inner) };
    let mut out = String::from("\x1b[?25l");
    out.push_str(&format!("\x1b[{top};{left}H\x1b[0m{tl}{title}{tr}"));
    for (i, l) in content.iter().enumerate() {
        let dim = if l == footer { "\x1b[2m" } else { "" };
        out.push_str(&format!("\x1b[{};{left}H\x1b[0m{v}{dim}{}\x1b[0m{v}", top + 1 + i, fit(&format!(" {l}"), inner)));
    }
    out.push_str(&format!("\x1b[{};{left}H\x1b[0m{bl}{}{br}", top + h_total - 1, h.to_string().repeat(inner)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (row, col, text) of every positioned run.
    fn cells(out: &str) -> Vec<(usize, usize, String)> {
        let mut v: Vec<(usize, usize, String)> = Vec::new();
        let mut it = out.chars().peekable();
        while let Some(ch) = it.next() {
            if ch == '\x1b' {
                it.next();
                let mut seq = String::new();
                while let Some(c) = it.next_if(|c| !c.is_ascii_alphabetic()) {
                    seq.push(c);
                }
                if it.next() == Some('H') {
                    let (r, c) = seq.split_once(';').unwrap();
                    v.push((r.parse().unwrap(), c.parse().unwrap(), String::new()));
                }
            } else {
                v.last_mut().unwrap().2.push(ch);
            }
        }
        v
    }

    fn check(cols: u16, rows: u16) -> Vec<(usize, usize, String)> {
        let cs = cells(&render(cols, rows, 0x1d, false));
        assert!(!cs.is_empty());
        let widths: Vec<usize> = cs.iter().map(|c| c.2.chars().count()).collect();
        assert!(widths.iter().all(|w| *w == widths[0]), "{cs:?}");
        for (r, c, t) in &cs {
            assert!(*r >= 1 && *r <= rows as usize && *c >= 1 && c + t.chars().count() - 1 <= cols as usize, "{r} {c} {t:?}");
        }
        cs
    }

    #[test]
    fn centered_at_80x24() {
        let cs = check(80, 24);
        let text: String = cs.iter().map(|c| c.2.clone()).collect::<Vec<_>>().join("\n");
        for want in ["C-] C-]", "C-] d", "C-] n / p", "C-] r", "C-] k", "C-] q", "RET attach", "any key closes"] {
            assert!(text.contains(want), "{want}\n{text}");
        }
        // vertically centered
        let (first, last) = (cs.first().unwrap().0, cs.last().unwrap().0);
        assert_eq!(first - 1, 24 - last);
    }

    #[test]
    fn fits_40x10_truncated() {
        let cs = check(40, 10);
        assert!(cs.len() <= 10 && cs.iter().any(|c| c.2.contains('…')));
        assert!(cs.iter().any(|c| c.2.contains("C-] C-]")));
    }

    #[test]
    fn tiny_screens_do_not_panic() {
        for (c, r) in [(5, 2), (10, 3), (20, 4), (30, 5)] {
            let _ = render(c, r, 0x1d, true);
        }
    }

    #[test]
    fn prefix_names() {
        assert_eq!(prefix_name(0x1d), "C-]");
        assert_eq!(prefix_name(0x02), "C-b");
    }
}
