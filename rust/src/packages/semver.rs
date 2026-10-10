use anyhow::{Result, bail};
use regex::Regex;
use std::cmp::Ordering;
use std::sync::OnceLock;

fn semver_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?$").unwrap()
    })
}
pub(crate) fn valid_package_name(name: &str) -> bool {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN
        .get_or_init(|| Regex::new(r"^[a-z0-9][a-z0-9._-]*$").unwrap())
        .is_match(name)
}
#[derive(Debug, Clone, Default)]
pub(crate) struct Version {
    pub major: String,
    pub minor: String,
    pub patch: String,
    pub pre: Vec<String>,
}
/// Returns the parsed parts together with any fault. Comparisons order by whatever parts
/// were recognized (an unparsable version sorts as the empty version), so both are exposed.
pub(crate) fn parse_version_lenient(s: &str) -> (Version, Option<String>) {
    let Some(captures) = semver_pattern().captures(s) else {
        return (
            Version::default(),
            Some(format!(
                "unsupported Unity package version {s:?}: require exact SemVer"
            )),
        );
    };
    let mut version = Version {
        major: captures[1].into(),
        minor: captures[2].into(),
        patch: captures[3].into(),
        pre: Vec::new(),
    };
    if let Some(pre) = captures.get(4).filter(|pre| !pre.as_str().is_empty()) {
        version.pre = pre.as_str().split('.').map(str::to_string).collect();
        if version
            .pre
            .iter()
            .any(|id| numeric(id) && id.len() > 1 && id.starts_with('0'))
        {
            return (
                version,
                Some(format!("invalid prerelease identifier in {s:?}")),
            );
        }
    }
    (version, None)
}
pub(crate) fn parse_version(s: &str) -> Result<Version> {
    match parse_version_lenient(s) {
        (version, None) => Ok(version),
        (_, Some(error)) => bail!(error),
    }
}
fn numeric(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|byte| byte.is_ascii_digit())
}
pub(crate) fn compare_version(a: &str, b: &str) -> Ordering {
    let (x, _) = parse_version_lenient(a);
    let (y, _) = parse_version_lenient(b);
    for (left, right) in [
        (&x.major, &y.major),
        (&x.minor, &y.minor),
        (&x.patch, &y.patch),
    ] {
        let order = left.len().cmp(&right.len()).then_with(|| left.cmp(right));
        if order != Ordering::Equal {
            return order;
        }
    }
    if x.pre.is_empty() && !y.pre.is_empty() {
        return Ordering::Greater;
    }
    if y.pre.is_empty() && !x.pre.is_empty() {
        return Ordering::Less;
    }
    for (left, right) in x.pre.iter().zip(&y.pre) {
        if left == right {
            continue;
        }
        match (numeric(left), numeric(right)) {
            (true, true) => {
                if left.len() != right.len() {
                    return left.len().cmp(&right.len());
                }
            }
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            (false, false) => {}
        }
        return left.cmp(right);
    }
    x.pre.len().cmp(&y.pre.len())
}
