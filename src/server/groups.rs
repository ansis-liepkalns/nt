//! Folder-tree grouping of sessions for the switcher. The server computes it (it knows $HOME and can look at the
//! disk); clients only display it.
//!
//! A session's *project root* is the outermost ancestor-or-self of its shell cwd that holds a `.git` (directory or
//! file) or a project marker file, searching only strictly below $HOME (when the cwd is under it) and never `/`,
//! `/home`, `/tmp`, `/root` or $HOME itself. Nested repos are absorbed by the outermost one. Sessions without a project
//! root are grouped when their cwds are nested; the anchor is the shallowest such cwd that is not too generic.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::proto::{GroupInfo, SessionId};

/// Files and directories that make a directory a project root (besides `.git`).
const MARKERS: &[&str] = &["Cargo.toml", "package.json", "pyproject.toml", "go.mod", "Makefile", "CMakeLists.txt", ".hg"];

/// Never a project root and never a group anchor.
const GENERIC: &[&str] = &["/", "/home", "/tmp", "/root"];

fn is_marked(dir: &Path) -> bool {
    dir.join(".git").exists() || MARKERS.iter().any(|m| dir.join(m).exists())
}

fn generic(dir: &Path, home: Option<&Path>) -> bool {
    home.is_some_and(|h| dir == h) || GENERIC.iter().any(|g| dir == Path::new(g))
}

/// The outermost ancestor-or-self of `cwd` that is a project root, if any.
pub fn project_root(cwd: &Path, home: Option<&Path>) -> Option<PathBuf> {
    if !cwd.is_absolute() {
        return None;
    }
    // Under $HOME only the directories strictly below it count (an outer `.git` in $HOME is a dotfiles repo).
    let under_home = home.is_some_and(|h| cwd != h && cwd.starts_with(h));
    cwd.ancestors()
        .take_while(|d| !under_home || home.is_some_and(|h| *d != h))
        .filter(|d| !generic(d, home))
        .filter(|d| is_marked(d))
        .last()
        .map(Path::to_path_buf)
}

/// Number of path components (`/etc` = 1).
fn depth(p: &Path) -> usize {
    p.components().count().saturating_sub(1)
}

fn info(key: &Path, home: Option<&Path>) -> GroupInfo {
    let shown_key = key.display().to_string();
    let rel = home.and_then(|h| key.strip_prefix(h).ok());
    let label = match rel {
        Some(r) if !r.as_os_str().is_empty() => key.file_name().map_or(shown_key.clone(), |n| n.to_string_lossy().into_owned()),
        _ => shown_key.clone(),
    };
    let shown = match rel {
        Some(r) if !r.as_os_str().is_empty() => format!("~/{}", r.display()),
        _ => shown_key.clone(),
    };
    GroupInfo { key: shown_key, label, shown }
}

/// The group of every session that has one. `cwds` are the sessions' shell working directories (empty = unknown).
pub fn assign(cwds: &[(SessionId, String)], home: &str) -> HashMap<SessionId, GroupInfo> {
    let home = (!home.is_empty() && home != "/").then(|| PathBuf::from(home));
    let home = home.as_deref();
    let mut out: HashMap<SessionId, GroupInfo> = HashMap::new();
    let mut roots: HashMap<PathBuf, Option<PathBuf>> = HashMap::new();
    let mut loose: Vec<(SessionId, PathBuf)> = Vec::new();
    let mut project_roots: HashSet<PathBuf> = HashSet::new();
    for (id, cwd) in cwds {
        let cwd = PathBuf::from(cwd);
        let root = roots.entry(cwd.clone()).or_insert_with(|| project_root(&cwd, home)).clone();
        match root {
            Some(r) => {
                project_roots.insert(r.clone());
                out.insert(*id, info(&r, home));
            }
            None if cwd.is_absolute() => loose.push((*id, cwd)),
            None => {}
        }
    }
    // Nested cwds without a project: anchor = the shallowest eligible cwd among the ancestors-or-self.
    let eligible = |a: &Path| {
        !generic(a, home) && depth(a) > 1 && !project_roots.iter().any(|r| r.starts_with(a))
    };
    let anchors: Vec<Option<PathBuf>> = loose
        .iter()
        .map(|(_, c)| {
            loose
                .iter()
                .map(|(_, a)| a)
                .filter(|a| c.starts_with(a) && eligible(a))
                .min_by_key(|a| depth(a))
                .cloned()
        })
        .collect();
    for (i, (id, _)) in loose.iter().enumerate() {
        let Some(a) = &anchors[i] else { continue };
        if anchors.iter().filter(|x| x.as_ref() == Some(a)).count() > 1 {
            out.insert(*id, info(a, home));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tree(PathBuf);

    impl Tree {
        fn new(tag: &str) -> Tree {
            let root = std::env::temp_dir().join(format!("nt-groups-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Tree(root.canonicalize().unwrap())
        }

        fn dir(&self, rel: &str) -> PathBuf {
            let p = self.0.join(rel);
            std::fs::create_dir_all(&p).unwrap();
            p
        }

        fn mark(&self, rel: &str, file: &str) {
            let p = self.dir(rel);
            if file.ends_with('/') {
                std::fs::create_dir_all(p.join(file)).unwrap();
            } else {
                std::fs::write(p.join(file), "").unwrap();
            }
        }

        fn s(&self, rel: &str) -> String {
            self.0.join(rel).display().to_string()
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn key(g: &HashMap<SessionId, GroupInfo>, id: SessionId) -> Option<String> {
        g.get(&id).map(|g| g.key.clone())
    }

    #[test]
    fn outermost_marked_ancestor_is_the_project() {
        let t = Tree::new("outer");
        let home = t.dir("home/a");
        t.mark("home/a/proj", ".git/");
        t.mark("home/a/proj/sub", ".git/"); // nested repo: absorbed
        t.mark("home/a/proj/sub/deep", "Cargo.toml");
        t.dir("home/a/proj/sub/deep/src");
        let h = Some(home.as_path());
        let root = t.0.join("home/a/proj");
        for rel in ["home/a/proj", "home/a/proj/sub", "home/a/proj/sub/deep/src"] {
            assert_eq!(project_root(&t.0.join(rel), h), Some(root.clone()), "{rel}");
        }
        // a `.git` file (worktree) counts, and so does every marker
        std::fs::write(t.dir("home/a/wt").join(".git"), "gitdir: x").unwrap();
        assert_eq!(project_root(&t.0.join("home/a/wt"), h), Some(t.0.join("home/a/wt")));
        for (i, m) in MARKERS.iter().enumerate() {
            let d = format!("home/a/m{i}");
            t.mark(&d, m);
            assert_eq!(project_root(&t.0.join(&d), h), Some(t.0.join(&d)), "{m}");
        }
        t.dir("home/a/plain");
        assert_eq!(project_root(&t.0.join("home/a/plain"), h), None);
    }

    #[test]
    fn home_and_generic_directories_are_never_roots() {
        let t = Tree::new("home");
        let home = t.dir("home/a");
        t.mark("home/a", ".git/"); // dotfiles repo in $HOME
        t.mark("home/a", "Makefile");
        t.dir("home/a/notes");
        t.mark("home/a/src/web", "package.json");
        let h = Some(home.as_path());
        assert_eq!(project_root(&home, h), None);
        assert_eq!(project_root(&t.0.join("home/a/notes"), h), None);
        assert_eq!(project_root(&t.0.join("home/a/src/web"), h), Some(t.0.join("home/a/src/web")));
        // a marker in /, /home, /tmp or /root is ignored
        assert_eq!(project_root(Path::new("/"), None), None);
        assert!(generic(Path::new("/tmp"), None) && generic(Path::new("/home"), None) && generic(Path::new("/root"), None));
        // outside $HOME a marked ancestor counts
        t.mark("srv/app", "go.mod");
        t.dir("srv/app/cmd");
        assert_eq!(project_root(&t.0.join("srv/app/cmd"), h), Some(t.0.join("srv/app")));
        // relative or empty cwd: nothing
        assert_eq!(project_root(Path::new("rel/dir"), h), None);
    }

    #[test]
    fn sessions_group_by_project_with_labels() {
        let t = Tree::new("proj");
        let home = t.dir("home/a");
        t.mark("home/a/proj-1", ".git/");
        t.mark("home/a/proj-1/sub", ".git/");
        t.mark("home/a/src/raytracer", "Cargo.toml");
        t.dir("home/a/src");
        let cwds = vec![
            (1, t.s("home/a/proj-1")),
            (2, t.s("home/a/proj-1/sub")),
            (3, t.s("home/a/src/raytracer")),
            (4, t.s("home/a/src")), // must not swallow raytracer, and is alone anyway
            (5, String::new()),
        ];
        let g = assign(&cwds, &home.display().to_string());
        assert_eq!(key(&g, 1), Some(t.s("home/a/proj-1")));
        assert_eq!(key(&g, 2), key(&g, 1));
        assert_eq!(g[&1].label, "proj-1");
        assert_eq!(g[&1].shown, "~/proj-1");
        assert_eq!(g[&3].label, "raytracer");
        assert_eq!(g[&3].shown, "~/src/raytracer");
        // single-session projects still get a group; the others do not
        assert_eq!(key(&g, 4), None);
        assert_eq!(key(&g, 5), None);
    }

    #[test]
    fn non_project_nesting_and_its_limits() {
        let t = Tree::new("loose");
        let home = t.dir("home/a");
        let h = home.display().to_string();
        // nested cwds: the shallowest is the anchor; outside $HOME the label is the whole path
        let cwds = vec![(1, t.s("etc/nginx")), (2, t.s("etc/nginx/conf.d")), (4, t.s("var/log"))];
        let g = assign(&cwds, &h);
        assert_eq!(key(&g, 1), Some(t.s("etc/nginx")));
        assert_eq!(key(&g, 2), key(&g, 1));
        assert_eq!(g[&1].label, t.s("etc/nginx"));
        assert_eq!(key(&g, 4), None);
        // a shallower session cwd takes over as the anchor
        let g = assign(&[(1, t.s("etc/nginx")), (2, t.s("etc/nginx/conf.d")), (3, t.s("etc"))], &h);
        assert_eq!((key(&g, 1), key(&g, 2)), (Some(t.s("etc")), Some(t.s("etc"))));
        // the same directory twice is a group
        let g = assign(&[(1, t.s("var/log")), (2, t.s("var/log"))], &h);
        assert_eq!(key(&g, 1), Some(t.s("var/log")));
        // inside $HOME: label = basename, and $HOME itself is never an anchor
        t.dir("home/a/notes/2024");
        let cwds = vec![(1, h.clone()), (2, t.s("home/a/notes")), (3, t.s("home/a/notes/2024")), (4, t.s("home/a/other"))];
        let g = assign(&cwds, &h);
        assert_eq!(key(&g, 1), None);
        assert_eq!(key(&g, 2), Some(t.s("home/a/notes")));
        assert_eq!(key(&g, 3), key(&g, 2));
        assert_eq!(g[&2].label, "notes");
        assert_eq!(g[&2].shown, "~/notes");
        assert_eq!(key(&g, 4), None);
        // a session in $HOME does not pull the others together
        let g = assign(&[(1, h.clone()), (2, t.s("home/a/x")), (3, t.s("home/a/y"))], &h);
        assert!(g.is_empty());
    }

    #[test]
    fn depth_one_and_tmp_anchors_are_refused() {
        let h = "/nonexistent-home/a";
        let g = assign(&[(1, "/etc".into()), (2, "/etc/x-nt-test".into()), (3, "/tmp".into()), (4, "/tmp".into()), (5, "/".into()), (6, "/".into())], h);
        // /etc is depth 1: the anchor becomes /etc/x-nt-test, alone, so nobody is grouped; /tmp and / are generic
        assert!(g.is_empty(), "{g:?}");
        let g = assign(&[(1, "/srv".into()), (2, "/srv".into()), (3, "/usr/share/x-nt-test".into()), (4, "/usr/share/x-nt-test/y".into())], h);
        assert_eq!(key(&g, 1), None, "depth-1 anchors are refused");
        assert_eq!(key(&g, 3), Some("/usr/share/x-nt-test".into()));
        assert_eq!(key(&g, 4), key(&g, 3));
    }

    #[test]
    fn a_shell_in_src_does_not_swallow_the_projects_below() {
        let t = Tree::new("swallow");
        let home = t.dir("home/a");
        let h = home.display().to_string();
        t.mark("home/a/src/webshop", ".git/");
        t.dir("home/a/src/other/deep");
        t.dir("home/a/src/other/deep2");
        let cwds = vec![(1, t.s("home/a/src")), (2, t.s("home/a/src/webshop")), (3, t.s("home/a/src/other/deep")), (4, t.s("home/a/src/other/deep2"))];
        let g = assign(&cwds, &h);
        assert_eq!(key(&g, 2), Some(t.s("home/a/src/webshop")));
        assert_eq!(key(&g, 1), None);
        // ~/src/other is not a session cwd: deep and deep2 are not nested in each other
        assert_eq!(key(&g, 3), None);
        assert_eq!(key(&g, 4), None);
        // but a shell in ~/src with no project below it anchors the nested ones
        let g = assign(&[(1, t.s("home/a/src")), (3, t.s("home/a/src/other/deep"))], &h);
        assert_eq!(key(&g, 1), Some(t.s("home/a/src")));
        assert_eq!(key(&g, 3), key(&g, 1));
    }

    #[test]
    fn groups_follow_the_cwd() {
        let t = Tree::new("follow");
        let home = t.dir("home/a");
        let h = home.display().to_string();
        t.mark("home/a/p", ".git/");
        t.dir("home/a/q");
        assert_eq!(key(&assign(&[(1, t.s("home/a/q"))], &h), 1), None);
        assert_eq!(key(&assign(&[(1, t.s("home/a/p"))], &h), 1), Some(t.s("home/a/p")));
    }
}
