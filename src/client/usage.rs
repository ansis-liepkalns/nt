//! Per-session CPU / memory display for the switcher: bars scaled to the busiest session, a one-glyph
//! "heat" for narrow terminals, and the ASCII fallbacks.

use crate::proto::SessionInfo;

const MIN_CPU: f32 = 100.0;
const MIN_RSS: u64 = 512 << 20;
const PARTIAL: [char; 7] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉'];

/// What a full bar means: the maximum in the list, but at least 100% CPU / 512 MiB.
pub struct Scale {
    cpu: f32,
    rss: u64,
}

impl Scale {
    pub fn of(list: &[SessionInfo]) -> Scale {
        Scale {
            cpu: list.iter().map(|s| s.cpu_pct).fold(MIN_CPU, f32::max),
            rss: list.iter().map(|s| s.rss_bytes).fold(MIN_RSS, u64::max),
        }
    }

    fn cpu_rel(&self, s: &SessionInfo) -> f64 {
        (s.cpu_pct / self.cpu).clamp(0.0, 1.0) as f64
    }

    fn rss_rel(&self, s: &SessionInfo) -> f64 {
        (s.rss_bytes as f64 / self.rss as f64).clamp(0.0, 1.0)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layout {
    /// Bars with numbers.
    Wide,
    /// Bars only.
    Mid,
    /// One heat glyph.
    Narrow,
}

pub fn layout(cols: usize) -> Layout {
    match cols {
        70.. => Layout::Wide,
        50..=69 => Layout::Mid,
        _ => Layout::Narrow,
    }
}

/// `NT_ASCII=1`, or a locale that is not UTF-8.
pub fn ascii_mode() -> bool {
    if std::env::var("NT_ASCII").is_ok_and(|v| !v.is_empty() && v != "0") {
        return true;
    }
    let loc = ["LC_ALL", "LC_CTYPE", "LANG"].iter().filter_map(|k| std::env::var(k).ok()).find(|v| !v.is_empty());
    !loc.is_some_and(|l| {
        let l = l.to_lowercase();
        l.contains("utf-8") || l.contains("utf8")
    })
}

pub fn fmt_rss(b: u64) -> String {
    const K: u64 = 1 << 10;
    match b {
        0 => "0".into(),
        b if b < K * K => format!("{}K", (b / K).max(1)),
        b if b < K * K * K => format!("{}M", b / (K * K)),
        b => {
            let g = b as f64 / (K * K * K) as f64;
            if g < 10.0 { format!("{g:.1}G") } else { format!("{g:.0}G") }
        }
    }
}

pub fn fmt_cpu(p: f32) -> String {
    format!("{}%", (p.round() as u32).min(9999))
}

/// A bar `frac` full (0..=1) with `cells` cells: ` ██▍  ` (unframed, so idle rows stay blank), or `[## ]` in ASCII mode.
pub fn bar(frac: f64, cells: usize, ascii: bool) -> String {
    let frac = frac.clamp(0.0, 1.0);
    let (open, close) = if ascii { ('[', ']') } else { (' ', ' ') };
    let mut s = String::from(open);
    if ascii {
        let n = ((frac * cells as f64).round() as usize).max(usize::from(frac > 0.005)).min(cells);
        s.push_str(&"#".repeat(n));
        s.push_str(&" ".repeat(cells - n));
    } else {
        let e = ((frac * (cells * 8) as f64).round() as usize).max(usize::from(frac > 0.005)).min(cells * 8);
        s.push_str(&"█".repeat(e / 8));
        if e % 8 > 0 {
            s.push(PARTIAL[e % 8 - 1]);
        }
        s.push_str(&" ".repeat(cells - e.div_ceil(8)));
    }
    s.push(close);
    s
}

/// One-glyph load indicator for `rel` in 0..=1.
pub fn heat(rel: f64, ascii: bool) -> char {
    let glyphs: [char; 5] = if ascii { [' ', '.', ':', '*', '#'] } else { [' ', '▁', '▃', '▅', '▇'] };
    let i = match rel {
        r if r < 0.02 => 0,
        r if r < 0.25 => 1,
        r if r < 0.5 => 2,
        r if r < 0.75 => 3,
        _ => 4,
    };
    glyphs[i]
}

/// The usage cell for a session's name line (with colour escapes) and its display width.
/// `top` marks the session with the most memory.
pub fn cell(s: &SessionInfo, scale: &Scale, top: bool, layout: Layout, ascii: bool) -> (String, usize) {
    let (cr, rr) = (scale.cpu_rel(s), scale.rss_rel(s));
    let cpu_color = if s.cpu_pct > 150.0 {
        "\x1b[31m"
    } else if s.cpu_pct > 80.0 {
        "\x1b[33m"
    } else {
        ""
    };
    let cpu_end = if cpu_color.is_empty() { "" } else { "\x1b[39m" };
    let (ram_on, ram_off) = if top { ("\x1b[1m", "\x1b[22m") } else { ("\x1b[2m", "\x1b[22m") };
    match layout {
        Layout::Wide => {
            let text = format!(
                "{cpu_color}{}{cpu_end} {:>4} {ram_on}{}{ram_off} {:>5}",
                bar(cr, 3, ascii),
                fmt_cpu(s.cpu_pct),
                bar(rr, 3, ascii),
                fmt_rss(s.rss_bytes)
            );
            (text, 22)
        }
        Layout::Mid => (format!("{cpu_color}{}{cpu_end} {ram_on}{}{ram_off}", bar(cr, 2, ascii), bar(rr, 2, ascii)), 9),
        Layout::Narrow => (heat(cr.max(rr), ascii).to_string(), 1),
    }
}

/// `cpu 12% · 340M`
pub fn detail(s: &SessionInfo) -> String {
    format!("cpu {} · {}", fmt_cpu(s.cpu_pct), fmt_rss(s.rss_bytes))
}

/// `12 sessions · cpu 34% · 2.1G`
pub fn total(list: &[SessionInfo]) -> String {
    let cpu: f32 = list.iter().map(|s| s.cpu_pct).sum();
    let rss: u64 = list.iter().map(|s| s.rss_bytes).sum();
    format!("{} sessions · cpu {} · {}", list.len(), fmt_cpu(cpu), fmt_rss(rss))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars() {
        assert_eq!(bar(1.0, 3, false), " ███ ");
        assert_eq!(bar(0.0, 3, false), "     ");
        assert_eq!(bar(0.5, 3, false), " █▌  ");
        assert_eq!(bar(0.01, 2, false), " ▏  ");
        assert_eq!(bar(0.5, 4, true), "[##  ]");
        assert_eq!(bar(1.0, 2, true), "[##]");
        assert_eq!(bar(0.01, 3, true), "[#  ]");
        for f in [0.0, 0.1, 0.33, 0.9, 1.0, 7.0] {
            assert_eq!(bar(f, 3, false).chars().count(), 5);
            assert_eq!(bar(f, 3, true).chars().count(), 5);
        }
    }

    #[test]
    fn heat_glyphs() {
        assert_eq!((heat(0.0, false), heat(0.1, false), heat(0.6, false), heat(1.0, false)), (' ', '▁', '▅', '▇'));
        assert_eq!((heat(0.0, true), heat(0.3, true), heat(1.0, true)), (' ', ':', '#'));
    }

    #[test]
    fn numbers() {
        assert_eq!(fmt_rss(340 << 20), "340M");
        assert_eq!(fmt_rss(2254857830), "2.1G");
        assert_eq!(fmt_rss(20 << 30), "20G");
        assert_eq!(fmt_rss(5000), "4K");
        assert_eq!(fmt_cpu(12.3), "12%");
        assert_eq!(layout(100), Layout::Wide);
        assert_eq!(layout(60), Layout::Mid);
        assert_eq!(layout(40), Layout::Narrow);
    }
}
