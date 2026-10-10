//! Keeps the account and machine names out of the naming digest. The model sees `~` for the home directory, `user` for the
//! login name and `host` for the short hostname, so it cannot mistake them for topics (a login name that looks like a word
//! or acronym would otherwise leak into topics), and they never leave the machine through the API backend.

/// Who the digest must not name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    pub home: String,
    pub user: String,
    pub host: String,
}

/// Names shorter than this are left alone (too many false hits).
const MIN_NAME: usize = 3;

impl Identity {
    /// $HOME, the login name (`$USER`, `$LOGNAME`, else the last component of $HOME) and the short hostname.
    pub fn current() -> Identity {
        let home = std::env::var("HOME").unwrap_or_default();
        let user = ["USER", "LOGNAME"]
            .iter()
            .filter_map(|k| std::env::var(k).ok())
            .find(|v| !v.is_empty())
            .or_else(|| home.rsplit('/').next().filter(|b| !b.is_empty()).map(String::from))
            .unwrap_or_default();
        let host = nix::unistd::gethostname().ok().map(|h| h.to_string_lossy().into_owned()).unwrap_or_default();
        Identity { home, user, host }
    }

    /// `text` with the home directory shown as `~`, the user name as `user` and the short host name as `host`.
    pub fn scrub(&self, text: &str) -> String {
        let short = self.host.split('.').next().unwrap_or("");
        // Host before user: a host name that starts with the user name (`dana-laptop`) must go as a whole.
        let out = scrub_word(&scrub_home(text, &self.home), short, "host");
        scrub_word(&out, &self.user, "user")
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Replace `home` by `~` where it is a whole path prefix: not inside a longer path component on either side.
pub fn scrub_home(text: &str, home: &str) -> String {
    let home = home.trim_end_matches('/');
    if home.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut prev: Option<char> = None;
    while let Some(i) = rest.find(home) {
        let before = rest[..i].chars().next_back().or(prev);
        let after = rest[i + home.len()..].chars().next();
        let ok_before = before.is_none_or(|c| !(is_word(c) || matches!(c, '-' | '.' | '~' | '/')));
        let ok_after = after.is_none_or(|c| !(is_word(c) || c == '-'));
        out.push_str(&rest[..i]);
        if ok_before && ok_after {
            out.push('~');
        } else {
            out.push_str(home);
        }
        prev = Some(home.chars().next_back().unwrap_or('/'));
        rest = &rest[i + home.len()..];
    }
    out.push_str(rest);
    out
}

/// Replace `name` (and its lowercase form) by `to` where it stands as a whole word: neither neighbour is a letter,
/// digit or underscore. Names shorter than `MIN_NAME` characters are left alone.
pub fn scrub_word(text: &str, name: &str, to: &str) -> String {
    if name.chars().count() < MIN_NAME {
        return text.to_string();
    }
    let lower = name.to_lowercase();
    let mut forms = vec![name];
    if lower != name {
        forms.push(&lower);
    }
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    'scan: while i < text.len() {
        for f in &forms {
            if text[i..].starts_with(*f) {
                let before = text[..i].chars().next_back();
                let after = text[i + f.len()..].chars().next();
                if before.is_none_or(|c| !is_word(c)) && after.is_none_or(|c| !is_word(c)) {
                    out.push_str(to);
                    i += f.len();
                    continue 'scan;
                }
            }
        }
        let c = text[i..].chars().next().unwrap_or(' ');
        out.push(c);
        i += c.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(home: &str, user: &str, host: &str) -> Identity {
        Identity { home: home.into(), user: user.into(), host: host.into() }
    }

    #[test]
    fn home_becomes_tilde_everywhere() {
        let i = id("/home/dana", "dana", "box");
        assert_eq!(i.scrub("cwd: /home/dana/src/nt (git nt)"), "cwd: ~/src/nt (git nt)");
        assert_eq!(i.scrub("  [/home/dana] ls /home/dana/x"), "  [~] ls ~/x");
        assert_eq!(i.scrub("title: \"vim /home/dana/a.rs\""), "title: \"vim ~/a.rs\"");
        assert_eq!(i.scrub("| error: no such file /home/dana/.cache/x"), "| error: no such file ~/.cache/x");
        assert_eq!(i.scrub("earlier summary: Worked in /home/dana/proj."), "earlier summary: Worked in ~/proj.");
        // not another user's home, not a longer directory, not a deeper path that merely contains it
        let h = id("/home/dana", "", "");
        assert_eq!(h.scrub("/home/dana2/x /home/dana-old /mnt/home/dana/x"), "/home/dana2/x /home/dana-old /mnt/home/dana/x");
        // with the user name also set, the remaining name occurrences are placeholders
        assert_eq!(i.scrub("/home/dana2/x /mnt/home/dana/x"), "/home/dana2/x /mnt/home/user/x");
        // trailing slash in $HOME, empty and root homes
        assert_eq!(id("/home/dana/", "", "").scrub("/home/dana/x"), "~/x");
        assert_eq!(id("", "", "").scrub("/home/dana/x"), "/home/dana/x");
        assert_eq!(id("/", "", "").scrub("/home/dana/x"), "/home/dana/x");
    }

    #[test]
    fn user_name_only_as_a_whole_word() {
        let i = id("/home/ann", "ann", "");
        assert_eq!(i.scrub("annotate ann ann@box /home/ann_x"), "annotate user user@box /home/ann_x");
        assert_eq!(i.scrub("annotate the manner"), "annotate the manner");
        assert_eq!(i.scrub("channel: ann."), "channel: user.");
        assert_eq!(i.scrub("x_ann ann_y ann2 2ann"), "x_ann ann_y ann2 2ann");
        // as a path component
        assert_eq!(i.scrub("/srv/ann/data /var/lib/annex"), "/srv/user/data /var/lib/annex");
        // case-sensitive: the name and its lowercase form
        let i = id("/home/Dana", "Dana", "");
        assert_eq!(i.scrub("Dana dana DANA ANSI escapes"), "user user DANA ANSI escapes");
        let i = id("", "dana", "");
        assert_eq!(i.scrub("dana@x Dana"), "user@x Dana");
        // non-ASCII neighbours count as letters
        assert_eq!(id("", "ann", "").scrub("éann annè"), "éann annè");
    }

    #[test]
    fn short_names_are_left_alone() {
        let i = id("", "al", "x1");
        assert_eq!(i.scrub("al al x1 x1"), "al al x1 x1");
        let i = id("", "abc", "defg");
        assert_eq!(i.scrub("abc defg"), "user host");
        assert_eq!(id("", "", "").scrub("anything"), "anything");
    }

    #[test]
    fn short_hostname_becomes_host() {
        let i = id("/home/u", "someone", "web02.example.org");
        assert_eq!(i.scrub("ssh web02 ; web02.example.org web02x"), "ssh host ; host.example.org web02x");
        assert_eq!(i.scrub("someone@web02:~$ ls"), "user@host:~$ ls");
        // the home path goes first, so a user name in it does not leave `~/user`
        let i = id("/home/dana", "dana", "dana-laptop");
        assert_eq!(i.scrub("/home/dana/dana-laptop dana"), "~/host user");
    }

    #[test]
    fn the_whole_digest_is_scrubbed() {
        let i = id("/home/dana", "dana", "box1");
        let d = "earlier summary: Tried dana scripts in /home/dana/src.\ncwd: /home/dana/src/ansi-demo (git ansi-demo)\ncommands (oldest first):\n  [/home/dana/src] ls /home/dana      ok  0s\n      | dana@box1: permission denied\nforeground: bash\n";
        let s = i.scrub(d);
        assert!(!s.contains("dana") && !s.contains("box1"), "{s}");
        assert!(s.contains("[~/src] ls ~ ") && s.contains("user@host: permission denied") && s.contains("ansi-demo"), "{s}");
    }
}
