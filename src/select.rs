//! Dataset selection by include/exclude globs with minimatch semantics: `*` and `?` stay within one
//! path element, `**` spans elements, `{a,b}` alternates, `[...]` classes.

use crate::util::split_list;
use anyhow::{Context, Result};
use globset::{GlobBuilder, GlobMatcher};

#[derive(Debug, Default, Clone)]
pub struct Selector {
    include: Vec<GlobMatcher>,
    exclude: Vec<GlobMatcher>,
}

fn compile(pattern: &str) -> Result<GlobMatcher> {
    Ok(GlobBuilder::new(pattern)
        .literal_separator(true)
        .backslash_escape(true)
        .build()
        .with_context(|| format!("invalid glob '{pattern}'"))?
        .compile_matcher())
}

impl Selector {
    /// Build from comma-separated include and exclude lists (either may be empty).
    pub fn new(include: &str, exclude: &str) -> Result<Selector> {
        Ok(Selector {
            include: split_list(include).iter().map(|p| compile(p)).collect::<Result<_>>()?,
            exclude: split_list(exclude).iter().map(|p| compile(p)).collect::<Result<_>>()?,
        })
    }

    /// True when the name matches any include glob (or there are none) and no exclude glob.
    pub fn matches(&self, name: &str) -> bool {
        (self.include.is_empty() || self.include.iter().any(|g| g.is_match(name)))
            && !self.exclude.iter().any(|g| g.is_match(name))
    }

    pub fn filter<T, F: Fn(&T) -> &str>(&self, items: Vec<T>, name: F) -> Vec<T> {
        items.into_iter().filter(|i| self.matches(name(i))).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [&str; 6] = ["pool1", "pool1/sub1", "pool1/sub1/swap", "pool1/sub2", "pool2", "pool2/sub3"];

    fn pick(include: &str, exclude: &str) -> Vec<&'static str> {
        let s = Selector::new(include, exclude).unwrap();
        ALL.iter().copied().filter(|n| s.matches(n)).collect()
    }

    #[test]
    fn readme_examples() {
        assert_eq!(pick("", ""), ALL.to_vec());
        assert_eq!(pick("pool1/**", ""), vec!["pool1/sub1", "pool1/sub1/swap", "pool1/sub2"]);
        assert_eq!(pick("pool1/*", "*/*2"), vec!["pool1/sub1"]);
    }

    #[test]
    fn star_does_not_cross_slash() {
        assert_eq!(pick("*", ""), vec!["pool1", "pool2"]);
        assert_eq!(pick("pool?/*", ""), vec!["pool1/sub1", "pool1/sub2", "pool2/sub3"]);
        assert_eq!(pick("**/swap", ""), vec!["pool1/sub1/swap"]);
    }

    #[test]
    fn lists_and_braces() {
        assert_eq!(pick("pool1/sub1,pool2", ""), vec!["pool1/sub1", "pool2"]);
        assert_eq!(pick("pool{1,2}/sub[13]", ""), vec!["pool1/sub1", "pool2/sub3"]);
        // `pool2*` does not cross `/`, so pool2/sub3 stays (minimatch semantics)
        assert_eq!(pick("**", "*/*/swap,pool2*"), vec!["pool1", "pool1/sub1", "pool1/sub2", "pool2/sub3"]);
        assert_eq!(pick("**", "*/*/swap,pool2,pool2/**"), vec!["pool1", "pool1/sub1", "pool1/sub2"]);
    }

    #[test]
    fn bad_glob_is_an_error() {
        assert!(Selector::new("pool[", "").is_err());
    }
}
