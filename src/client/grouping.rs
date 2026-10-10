//! Display helpers for the folder-tree groups (`SessionInfo.group`), shared by the switcher and `nt ls`.

use crate::proto::{GroupInfo, NameSource, SessionInfo};

/// Status icons, worst first.
const ICONS: [char; 4] = ['✗', '⏵', '✎', '✓'];

/// The icon at the start of a live session's status line (`✗ cargo test (101) · 5s ago`).
fn icon(s: &SessionInfo) -> Option<char> {
    if s.exited {
        return None;
    }
    s.status.chars().next().filter(|c| ICONS.contains(c))
}

/// The distinct status icons of `members`, worst first (`✗` then `⏵` then `✎` then `✓`).
pub fn icons<'a>(members: impl IntoIterator<Item = &'a SessionInfo>) -> Vec<char> {
    let seen: Vec<char> = members.into_iter().filter_map(icon).collect();
    ICONS.iter().copied().filter(|c| seen.contains(c)).collect()
}

/// SGR colour code of a status icon.
pub fn icon_color(c: char) -> &'static str {
    match c {
        '✓' => "32",
        '✗' => "31",
        _ => "33",
    }
}

/// The session's name as shown inside its group: a heuristic `label:cmd` loses the redundant `label:`, and a bare
/// `label` becomes the foreground command.
pub fn display_name(s: &SessionInfo, group: Option<&GroupInfo>) -> String {
    let (Some(g), NameSource::Heuristic) = (group, s.name_source) else { return s.name.clone() };
    match s.name.strip_prefix(g.label.as_str()) {
        Some("") if !s.fg_cmd.is_empty() => s.fg_cmd.clone(),
        Some("") => "-".into(),
        Some(rest) => match rest.strip_prefix(':') {
            Some(cmd) if !cmd.is_empty() => cmd.to_string(),
            _ => s.name.clone(),
        },
        None => s.name.clone(),
    }
}

/// The session's directory relative to its group root with a trailing `/` (`sub/dir/`); empty at the root.
/// Falls back to the full cwd when it is not below the root.
pub fn subdir(s: &SessionInfo, g: &GroupInfo) -> String {
    match s.cwd.strip_prefix(g.key.as_str()) {
        Some("") => String::new(),
        Some(rest) if rest.starts_with('/') => format!("{}/", rest.trim_matches('/')),
        _ => s.cwd.clone(),
    }
}

/// A status line as shown inside a group: `idle · <path>` drops the path (the header has it), keeping only the subdirectory.
pub fn member_status(text: &str, s: &SessionInfo, g: Option<&GroupInfo>) -> String {
    match (g, text.strip_prefix("idle · ")) {
        (Some(g), Some(_)) => match subdir(s, g).as_str() {
            "" => "idle".into(),
            sub => format!("idle · {sub}"),
        },
        _ => text.to_string(),
    }
}

/// Groups for a listing: each group with its members, then the ungrouped sessions (`group: None`).
/// Groups by most recent use of any member, members by recency.
pub struct Block<'a> {
    pub group: Option<&'a GroupInfo>,
    pub members: Vec<&'a SessionInfo>,
}

pub fn blocks(sessions: &[SessionInfo]) -> Vec<Block<'_>> {
    let mut v: Vec<&SessionInfo> = sessions.iter().collect();
    v.sort_by_key(|s| std::cmp::Reverse(s.last_used_unix));
    let mut out: Vec<Block> = Vec::new();
    for s in &v {
        if let Some(g) = &s.group {
            match out.iter_mut().find(|b| b.group.is_some_and(|x| x.key == g.key)) {
                Some(b) => b.members.push(s),
                None => out.push(Block { group: Some(g), members: vec![s] }),
            }
        }
    }
    let loose: Vec<&SessionInfo> = v.into_iter().filter(|s| s.group.is_none()).collect();
    if !loose.is_empty() {
        out.push(Block { group: None, members: loose });
    }
    out
}

/// The header line of a group in `nt ls`: `webshop  ~/work/webshop  ✗ ✓` (no colour: the output is often piped).
pub fn header_text(g: &GroupInfo, members: &[&SessionInfo]) -> String {
    let mut t = g.label.clone();
    if g.shown != g.label {
        t.push_str(&format!("  {}", g.shown));
    }
    let ic: Vec<String> = icons(members.iter().copied()).iter().map(char::to_string).collect();
    if !ic.is_empty() {
        t.push_str(&format!("  {}", ic.join(" ")));
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(id: u32, name: &str, src: NameSource, cwd: &str, status: &str, used: u64, g: Option<&GroupInfo>) -> SessionInfo {
        SessionInfo {
            id,
            name: name.into(),
            name_source: src,
            created_unix: 0,
            last_active_unix: used,
            last_used_unix: used,
            clients: 0,
            cols: 80,
            rows: 24,
            cwd: cwd.into(),
            fg_cmd: "bash".into(),
            recap: String::new(),
            summary: String::new(),
            exited: false,
            status: status.into(),
            status_since: 0,
            cpu_pct: 0.0,
            rss_bytes: 0,
            pending: false,
            branch: String::new(),
            summary_unix: 0,
            summary_stale: false,
            group: g.cloned(),
        }
    }

    pub fn grp(key: &str, label: &str, shown: &str) -> GroupInfo {
        GroupInfo { key: key.into(), label: label.into(), shown: shown.into() }
    }

    #[test]
    fn icons_worst_first_deduped() {
        let g = grp("/h/a", "a", "~/a");
        let l = [
            s(1, "x", NameSource::Initial, "/h/a", "✓ ok · {ago}", 1, Some(&g)),
            s(2, "y", NameSource::Initial, "/h/a", "⏵ make", 1, Some(&g)),
            s(3, "z", NameSource::Initial, "/h/a", "✗ cargo test", 1, Some(&g)),
            s(4, "w", NameSource::Initial, "/h/a", "✓ ok again", 1, Some(&g)),
            s(5, "v", NameSource::Initial, "/h/a", "idle · ~/a", 1, Some(&g)),
            s(6, "u", NameSource::Initial, "/h/a", "✎ vim x", 1, Some(&g)),
        ];
        assert_eq!(icons(&l), vec!['✗', '⏵', '✎', '✓']);
        assert_eq!(icons(&l[3..5]), vec!['✓']);
        let mut ex = l[2].clone();
        ex.exited = true;
        assert!(icons([&ex]).is_empty());
    }

    #[test]
    fn names_and_subdirs() {
        let g = grp("/h/a/web", "web", "~/a/web");
        let heur = s(1, "web:cargo-test", NameSource::Heuristic, "/h/a/web/api/src", "", 1, Some(&g));
        assert_eq!(display_name(&heur, Some(&g)), "cargo-test");
        assert_eq!(display_name(&heur, None), "web:cargo-test");
        assert_eq!(display_name(&s(1, "web", NameSource::Heuristic, "", "", 1, Some(&g)), Some(&g)), "bash");
        let w = s(1, "x", NameSource::Initial, "/h/a/web/api", "idle · ~/a/web/api", 1, Some(&g));
        assert_eq!(member_status(&w.status, &w, Some(&g)), "idle · api/");
        assert_eq!(member_status("idle · ~/a/web", &s(1, "x", NameSource::Initial, "/h/a/web", "", 1, None), Some(&g)), "idle");
        assert_eq!(member_status("idle · ~/a/web", &w, None), "idle · ~/a/web");
        assert_eq!(member_status("✓ ok", &w, Some(&g)), "✓ ok");
        // LLM and user names are never touched, and neither is a prefix that is merely similar
        assert_eq!(display_name(&s(1, "web:x", NameSource::Llm, "", "", 1, Some(&g)), Some(&g)), "web:x");
        assert_eq!(display_name(&s(1, "website:vim", NameSource::Heuristic, "", "", 1, Some(&g)), Some(&g)), "website:vim");
        assert_eq!(subdir(&heur, &g), "api/src/");
        assert_eq!(subdir(&s(1, "x", NameSource::Initial, "/h/a/web", "", 1, None), &g), "");
        assert_eq!(subdir(&s(1, "x", NameSource::Initial, "/h/a/webby", "", 1, None), &g), "/h/a/webby");
    }

    #[test]
    fn blocks_order_and_header_text() {
        let a = grp("/h/a", "a", "~/a");
        let b = grp("/etc/nginx", "/etc/nginx", "/etc/nginx");
        let l = vec![
            s(1, "x", NameSource::Initial, "/h/a", "✓ ok", 10, Some(&a)),
            s(2, "y", NameSource::Initial, "/tmp", "", 99, None),
            s(3, "z", NameSource::Initial, "/etc/nginx", "✗ bad", 50, Some(&b)),
            s(4, "w", NameSource::Initial, "/h/a/sub", "", 20, Some(&a)),
        ];
        let bl = blocks(&l);
        let ids: Vec<Vec<u32>> = bl.iter().map(|b| b.members.iter().map(|s| s.id).collect()).collect();
        assert_eq!(ids, vec![vec![3], vec![4, 1], vec![2]]);
        assert!(bl[2].group.is_none());
        assert_eq!(header_text(&b, &bl[0].members), "/etc/nginx  ✗");
        assert_eq!(header_text(&a, &bl[1].members), "a  ~/a  ✓");
    }
}
