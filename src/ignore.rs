//! Ignore patterns: a `.rustsyncignore` file in the top folder of a library lists what is not
//! synced, one pattern per line, much like a `.gitignore`:
//!
//! ```text
//! # build output
//! target/
//! *.tmp
//! /notes/private
//! docs/**/*.pdf
//! !keep.tmp
//! ```
//!
//! - a pattern without a `/` matches a name anywhere (`*.tmp`, `node_modules`);
//! - a pattern with a `/` (other than at the end) matches the path from the top of the library
//!   (`/notes/private`, `docs/**/*.pdf`);
//! - a `/` at the end matches folders only (`target/`);
//! - `*` matches anything except `/`, `**` anything including `/`, `?` one character;
//! - a line starting with `!` takes a file back in (but not one inside an ignored folder);
//!   the last pattern that matches decides; `#` starts a comment.
//!
//! The file itself is synced, so the same patterns apply on every device. Ignored files are not
//! sent, not fetched and not deleted anywhere: whatever copies already exist stay where they are.

/// The name of the file, in the top folder of a library.
pub const IGNORE_FILE: &str = ".rustsyncignore";
/// Limits on what is read from it.
pub const MAX_PATTERNS: usize = 500;
pub const MAX_PATTERN_LEN: usize = 300;
pub const MAX_FILE_LEN: u64 = 64 * 1024;

#[derive(Debug, Clone)]
struct Pattern {
    glob: Vec<char>,
    /// Matches the whole path from the top (else: just the name).
    anchored: bool,
    dirs_only: bool,
    negated: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Ignore {
    patterns: Vec<Pattern>,
    /// The lines that make up the patterns, as written.
    lines: Vec<String>,
}

impl Ignore {
    pub fn parse(text: &str) -> Ignore {
        let mut ig = Ignore::default();
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.chars().count() > MAX_PATTERN_LEN {
                continue;
            }
            if line.chars().any(|c| c.is_control()) {
                continue;
            }
            if ig.patterns.len() >= MAX_PATTERNS {
                break;
            }
            let (negated, rest) = match line.strip_prefix('!') {
                Some(r) => (true, r),
                None => (false, line),
            };
            let (dirs_only, rest) = match rest.strip_suffix('/') {
                Some(r) => (true, r),
                None => (false, rest),
            };
            let anchored = rest.contains('/');
            let rest = rest.trim_start_matches('/');
            if rest.is_empty() {
                continue;
            }
            ig.patterns.push(Pattern { glob: rest.chars().collect(), anchored, dirs_only, negated });
            ig.lines.push(line.to_string());
        }
        ig
    }

    /// The patterns, as written (comments and blank lines left out).
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Is this path (relative to the top of the library) ignored? `is_dir`: is it a folder.
    /// Anything inside an ignored folder is ignored too.
    pub fn is_ignored(&self, rel: &str, is_dir: bool) -> bool {
        if self.patterns.is_empty() || rel.is_empty() || rel == IGNORE_FILE {
            return false;
        }
        let parts: Vec<&str> = rel.split('/').collect();
        let mut end = 0usize;
        for (i, part) in parts.iter().enumerate() {
            end += part.len() + if i > 0 { 1 } else { 0 };
            let prefix = &rel[..end];
            let dir = i + 1 < parts.len() || is_dir;
            if self.decide(prefix, part, dir) == Some(true) {
                return true;
            }
        }
        false
    }

    /// What the last matching pattern says about one path: Some(true) ignored, Some(false) taken
    /// back in, None no pattern matches.
    fn decide(&self, path: &str, name: &str, is_dir: bool) -> Option<bool> {
        let path: Vec<char> = path.chars().collect();
        let name: Vec<char> = name.chars().collect();
        let mut result = None;
        for p in &self.patterns {
            if p.dirs_only && !is_dir {
                continue;
            }
            let text = if p.anchored { &path } else { &name };
            if glob(&p.glob, text) {
                result = Some(!p.negated);
            }
        }
        result
    }
}

/// `*` anything but `/`, `**` anything, `?` one character but `/`.
fn glob(p: &[char], t: &[char]) -> bool {
    // Iterative matching with backtracking points for `*` and `**`, so long names cannot make
    // it slow.
    fn go(p: &[char], t: &[char], depth: usize) -> bool {
        if depth > 64 {
            return false;
        }
        let (mut pi, mut ti) = (0usize, 0usize);
        let mut star: Option<(usize, usize)> = None; // (pattern index after `*`, text index)
        while ti < t.len() {
            if pi + 1 < p.len() && p[pi] == '*' && p[pi + 1] == '*' {
                // `**`: also swallow a following `/` so `a/**/b` matches `a/b`.
                let mut rest = pi + 2;
                if rest < p.len() && p[rest] == '/' {
                    rest += 1;
                }
                return (ti..=t.len()).any(|k| go(&p[rest..], &t[k..], depth + 1));
            }
            if pi < p.len() && p[pi] == '*' {
                star = Some((pi + 1, ti));
                pi += 1;
                continue;
            }
            if pi < p.len() && (p[pi] == t[ti] && t[ti] != '/' || p[pi] == '?' && t[ti] != '/' || p[pi] == '/' && t[ti] == '/') {
                pi += 1;
                ti += 1;
                continue;
            }
            match star {
                Some((sp, st)) if t[st] != '/' => {
                    star = Some((sp, st + 1));
                    pi = sp;
                    ti = st + 1;
                }
                _ => return false,
            }
        }
        while pi < p.len() && p[pi] == '*' {
            pi += 1;
        }
        pi == p.len()
    }
    go(p, t, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ig(text: &str) -> Ignore {
        Ignore::parse(text)
    }

    #[test]
    fn globs() {
        let g = |p: &str, t: &str| glob(&p.chars().collect::<Vec<_>>(), &t.chars().collect::<Vec<_>>());
        assert!(g("*.tmp", "a.tmp"));
        assert!(!g("*.tmp", "a.tmpx"));
        assert!(!g("*.tmp", "d/a.tmp"));
        assert!(g("a?c", "abc") && !g("a?c", "a/c"));
        assert!(g("docs/**/*.pdf", "docs/x.pdf"));
        assert!(g("docs/**/*.pdf", "docs/a/b/x.pdf"));
        assert!(!g("docs/**/*.pdf", "other/x.pdf"));
        assert!(g("**", "a/b/c"));
        assert!(g("a/*/c", "a/b/c") && !g("a/*/c", "a/b/x/c"));
        assert!(g("*", "") && !g("?", ""));
        // a long name with many stars stays quick
        let long = "a".repeat(5000);
        assert!(!g("*a*a*a*a*a*a*a*b", &long));
    }

    #[test]
    fn names_paths_and_folders() {
        let i = ig("# build output\ntarget/\n*.tmp\n/notes/private\ndocs/**/*.pdf\n\n!keep.tmp\n");
        assert_eq!(i.lines().len(), 5);
        // a folder pattern: the folder and everything in it, at any depth
        assert!(i.is_ignored("target", true));
        assert!(i.is_ignored("target/debug/build/x.o", false));
        assert!(i.is_ignored("crates/a/target/x", false));
        assert!(!i.is_ignored("target", false)); // a file called target is not a folder
        // names anywhere
        assert!(i.is_ignored("a.tmp", false) && i.is_ignored("x/y/a.tmp", false));
        // taken back in
        assert!(!i.is_ignored("keep.tmp", false) && !i.is_ignored("x/keep.tmp", false));
        // anchored paths
        assert!(i.is_ignored("notes/private", false) && i.is_ignored("notes/private/a", false));
        assert!(!i.is_ignored("x/notes/private", false));
        assert!(i.is_ignored("docs/a/b.pdf", false) && !i.is_ignored("docs/a/b.txt", false));
        // the ignore file itself is always synced
        assert!(!ig("*\n").is_ignored(IGNORE_FILE, false));
        assert!(ig("*\n").is_ignored("anything", false));
    }

    #[test]
    fn nothing_inside_an_ignored_folder_comes_back() {
        let i = ig("build/\n!build/keep\n");
        assert!(i.is_ignored("build/keep", false));
    }

    #[test]
    fn junk_lines_are_skipped() {
        let i = ig("/\n!\n#x\n   \n  ok  \nbad\u{7}line\n");
        assert_eq!(i.lines(), ["ok"]);
        let many: String = (0..MAX_PATTERNS + 50).map(|n| format!("p{}\n", n)).collect();
        assert_eq!(ig(&many).lines().len(), MAX_PATTERNS);
        assert!(ig("").is_empty() && !ig("").is_ignored("a", false));
    }
}
