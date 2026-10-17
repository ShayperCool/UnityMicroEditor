//! Managed plugin metadata, plugin references,
//! response files and compiler arguments, Unity/package version expressions.
use super::{plugin_yaml, sorted_strings, symbol};
use crate::{
    compiler::{AssemblyConfig, NilMap, NilVec, editor_source, is_not_exist},
    project::{lexpath::rune_lower, strconv},
    sha256, sysutil,
    util::{read_agent_file, within_directory},
};
use anyhow::{Result, anyhow, bail};
use regex::Regex;
use std::{collections::BTreeMap, sync::LazyLock};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourcePlugin {
    pub path: String,
    pub enabled: bool,
    pub explicit: bool,
    pub constraints: NilVec<String>,
}

/// The Linux Player state of one managed plugin.
pub fn read_source_plugin(path: &str, defines: &[String]) -> Result<SourcePlugin> {
    let mut result = SourcePlugin {
        path: path.to_string(),
        ..Default::default()
    };
    let content = match read_agent_file(&format!("{path}.meta"), 32 << 20) {
        Ok(content) => content,
        Err(error) if is_not_exist(&error) => return Ok(result),
        Err(error) => return Err(error),
    };
    let Some(metadata) = plugin_yaml::decode(&content)
        .map_err(|error| anyhow!("plugin metadata {path}: {error}"))?
    else {
        // GUID-only metadata keeps Unity's default managed-plugin importer:
        // all Player platforms, unless the asset is in an Editor folder.
        result.enabled = !editor_source(path);
        return Ok(result);
    };
    result.explicit = metadata.explicit != 0;
    result.constraints = metadata.constraints;
    let (mut any_enabled, mut explicit_linux, mut linux_enabled) = (false, false, false);
    for platform in &metadata.platforms {
        for (group, target) in &platform.first {
            if target == "Any" || group == "Any" {
                any_enabled = platform.enabled != 0;
                if platform
                    .settings
                    .get("Exclude Linux64")
                    .is_some_and(|value| value == "1")
                {
                    any_enabled = false;
                }
            }
            if group == "Standalone" && target == "Linux64" {
                explicit_linux = true;
                linux_enabled = platform.enabled != 0;
            }
        }
    }
    result.enabled = any_enabled;
    if explicit_linux {
        result.enabled = linux_enabled;
    }
    let constraints = match_constraints(&result.constraints, defines)?;
    result.enabled = result.enabled && constraints;
    Ok(result)
}

pub fn plugin_references(
    config: &AssemblyConfig,
    plugins: &BTreeMap<String, SourcePlugin>,
) -> Result<NilVec<String>> {
    let mut result = config.compiler_references.clone();
    if config.config.override_references {
        for name in &config.config.precompiled {
            let Some(plugin) = plugins.get(name) else {
                bail!("explicit precompiled assembly is unavailable for Linux Mono: {name}");
            };
            result.push(plugin.path.clone());
        }
    } else {
        for plugin in plugins.values() {
            if !plugin.explicit {
                result.push(plugin.path.clone());
            }
        }
    }
    Ok(sorted_strings(&result))
}

/// A string built from bytes: every invalid byte reads as one U+FFFD.
pub fn lossy_runes(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len());
    let mut rest = bytes;
    loop {
        match std::str::from_utf8(rest) {
            Ok(valid) => {
                text.push_str(valid);
                return text;
            }
            Err(error) => {
                let (valid, invalid) = rest.split_at(error.valid_up_to());
                text.push_str(std::str::from_utf8(valid).unwrap_or_default());
                text.push('\u{fffd}');
                rest = &invalid[1..];
            }
        }
    }
}

pub fn split_arguments(text: &str) -> Result<NilVec<String>> {
    let mut result = NilVec::nil();
    let mut token = String::new();
    let (mut quoted, mut escaped) = (false, false);
    for character in text.chars() {
        if escaped {
            token.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' && quoted {
            escaped = true;
            continue;
        }
        if character == '"' {
            quoted = !quoted;
            continue;
        }
        if character.is_whitespace() && !quoted {
            if !token.is_empty() {
                result.push(std::mem::take(&mut token));
            }
            continue;
        }
        token.push(character);
    }
    if quoted || escaped {
        bail!("unterminated response-file argument");
    }
    if !token.is_empty() {
        result.push(token);
    }
    Ok(result)
}

fn warning_value(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b',' | b';'))
}

pub fn apply_response_files(
    project: &str,
    sources: &[String],
    authored_arguments: &[String],
    config: &mut AssemblyConfig,
    plugins: &BTreeMap<String, SourcePlugin>,
    inputs: &mut NilMap<String>,
) -> Result<()> {
    let mut paths = vec![sysutil::join(&[project, "Assets/csc.rsp"])];
    if let Some(first) = sources.first() {
        let mut common = sysutil::dir(first);
        for source in &sources[1..] {
            while !within_directory(source, &common) && sysutil::dir(&common) != common {
                common = sysutil::dir(&common);
            }
        }
        paths.push(sysutil::join(&[&common, "csc.rsp"]));
    }
    paths.sort();
    paths.dedup();
    let mut arguments = authored_arguments.to_vec();
    for path in paths {
        let content = match read_agent_file(&path, 1 << 20) {
            Ok(content) => content,
            Err(error) if is_not_exist(&error) => {
                inputs.insert(path, "absent".to_string());
                continue;
            }
            Err(error) => return Err(error),
        };
        inputs.insert(path.clone(), sha256(&content));
        config.response_files.push(path.clone());
        let parsed =
            split_arguments(&lossy_runes(&content)).map_err(|error| anyhow!("{path}: {error}"))?;
        arguments.extend(parsed.iter().cloned());
    }
    for argument in &arguments {
        let trimmed = argument.trim_start_matches(['-', '/']);
        let (option, value, has_value) = match trimmed.split_once(':') {
            Some((option, value)) => (option, value, true),
            None => (trimmed, "", false),
        };
        let lower = rune_lower(option);
        match lower.as_str() {
            "define" | "d" => {
                if !has_value {
                    bail!("response define requires value");
                }
                for define in value.split([';', ',']).filter(|define| !define.is_empty()) {
                    if !symbol(define) || define.starts_with("UNITY_EDITOR") {
                        bail!("invalid Player response define: {define}");
                    }
                    config.defines.push(define.to_string());
                }
            }
            "unsafe+" | "unsafe" => config.config.allow_unsafe = true,
            "unsafe-" => config.config.allow_unsafe = false,
            "reference" | "r" => {
                if !has_value || value.contains('=') {
                    bail!("response references require one unaliased managed plugin");
                }
                let name = sysutil::base(value);
                let Some(plugin) = plugins.get(&name) else {
                    bail!("response reference is outside active managed plugins: {value}");
                };
                if value != name && sysutil::join(&[project, value]) != plugin.path {
                    bail!("response reference does not match its canonical plugin: {value}");
                }
                config.compiler_references.push(plugin.path.clone());
            }
            "langversion" => {
                if !has_value || !(value == "9" || value == "9.0") {
                    bail!("pinned lifecycle SDK currently requires C#9 response langversion");
                }
            }
            "nullable" => {
                if !matches!(value, "enable" | "disable" | "warnings" | "annotations") {
                    bail!("invalid nullable option");
                }
                config.compiler_options.push(format!("-nullable:{value}"));
            }
            "nowarn" | "warn" | "w" | "warnaserror+" | "warnaserror-" | "warnaserror" => {
                if has_value && !warning_value(value) {
                    bail!("invalid warning compiler option");
                }
                let mut entry = format!("-{option}");
                if has_value {
                    entry = format!("{entry} :{value}").replace(" :", ":");
                }
                config.compiler_options.push(entry);
            }
            "optimize+" | "optimize-" | "checked+" | "checked-" | "deterministic+" => {
                config.compiler_options.push(format!("-{lower}"));
            }
            _ => bail!(
                "unsupported response/compiler option must not silently change SDK behavior: {argument}"
            ),
        }
    }
    config.defines = sorted_strings(&config.defines);
    config.response_files = sorted_strings(&config.response_files);
    Ok(())
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceVersion {
    pub numbers: [i64; 3],
    pub channel: String,
    pub iteration: i64,
    pub pre: String,
}

static VERSION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([0-9]+)(?:\.([0-9]+))?(?:\.([0-9]+))?(?:([abfpcx])([0-9]+)|-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?$").unwrap()
});

pub fn parse_version(text: &str) -> Result<SourceVersion> {
    let Some(matched) = VERSION.captures(text) else {
        bail!("invalid Unity/package version: {text}");
    };
    let group = |index: usize| matched.get(index).map_or("", |group| group.as_str());
    let mut result = SourceVersion::default();
    for index in 0..3 {
        if !group(index + 1).is_empty() {
            result.numbers[index] = strconv::atoi(group(index + 1))?;
        }
    }
    result.channel = group(4).to_string();
    result.iteration = strconv::atoi_lossy(group(5));
    result.pre = group(6).to_string();
    Ok(result)
}

fn channel_rank(channel: &str) -> i32 {
    match channel {
        "a" => 0,
        "b" => 1,
        "f" => 2,
        "p" | "c" | "x" => 3,
        _ => 4,
    }
}

/// -1, 0 or 1.
pub fn compare_versions(first: &SourceVersion, second: &SourceVersion) -> i32 {
    for index in 0..3 {
        if first.numbers[index] < second.numbers[index] {
            return -1;
        }
        if first.numbers[index] > second.numbers[index] {
            return 1;
        }
    }
    if first.pre != second.pre {
        if first.pre.is_empty() {
            return 1;
        }
        if second.pre.is_empty() {
            return -1;
        }
        let a: Vec<&str> = first.pre.split('.').collect();
        let b: Vec<&str> = second.pre.split('.').collect();
        for (a, b) in a.iter().zip(&b) {
            if a == b {
                continue;
            }
            return match (strconv::atoi(a), strconv::atoi(b)) {
                (Ok(a), Ok(b)) => {
                    if a < b {
                        -1
                    } else {
                        1
                    }
                }
                (Ok(_), Err(_)) => -1,
                (Err(_), Ok(_)) => 1,
                (Err(_), Err(_)) => {
                    if a < b {
                        -1
                    } else {
                        1
                    }
                }
            };
        }
        return if a.len() < b.len() { -1 } else { 1 };
    }
    let (first_rank, second_rank) = (channel_rank(&first.channel), channel_rank(&second.channel));
    if first_rank != second_rank {
        return if first_rank < second_rank { -1 } else { 1 };
    }
    match first.iteration.cmp(&second.iteration) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Greater => 1,
        std::cmp::Ordering::Equal => 0,
    }
}

/// A Unity version-define expression.
pub fn match_version(current: &str, expression: &str) -> Result<bool> {
    let version = parse_version(current)?;
    let expression = expression.trim();
    let bytes = expression.as_bytes();
    let Some(&open) = bytes.first() else {
        return Ok(true);
    };
    if open == b'[' || open == b'(' {
        let close = bytes[bytes.len() - 1];
        if bytes.len() < 2 || (close != b']' && close != b')') {
            bail!("invalid version interval: {expression}");
        }
        let body = &expression[1..expression.len() - 1];
        let Some((lower, upper)) = body.split_once(',') else {
            if open != b'[' || close != b']' {
                bail!("exact version must use brackets");
            }
            let exact = parse_version(body.trim())?;
            return Ok(compare_versions(&version, &exact) == 0);
        };
        if !lower.trim().is_empty() {
            let bound = parse_version(lower.trim())?;
            let comparison = compare_versions(&version, &bound);
            if comparison < 0 || comparison == 0 && open == b'(' {
                return Ok(false);
            }
        }
        if !upper.trim().is_empty() {
            let bound = parse_version(upper.trim())?;
            let comparison = compare_versions(&version, &bound);
            if comparison > 0 || comparison == 0 && close == b')' {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    let minimum = parse_version(expression)?;
    Ok(compare_versions(&version, &minimum) >= 0)
}

/// Every constraint needs one satisfied `||` term.
pub fn match_constraints(constraints: &[String], defines: &[String]) -> Result<bool> {
    for constraint in constraints {
        let mut matched = false;
        for term in constraint.split("||") {
            let term = term.trim();
            let negative = term.starts_with('!');
            let term = term.strip_prefix('!').unwrap_or(term);
            if !symbol(term) {
                bail!("unsupported define constraint: {constraint}");
            }
            let present = defines.iter().any(|define| define == term);
            matched = matched || (present != negative);
        }
        if !matched {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The Editor's state of one managed plugin: enabled for the Editor
/// platform, or for any platform the Editor is not excluded from.
pub fn read_editor_plugin(path: &str, defines: &[String]) -> Result<SourcePlugin> {
    let mut result = SourcePlugin {
        path: path.to_string(),
        ..Default::default()
    };
    let content = match read_agent_file(&format!("{path}.meta"), 32 << 20) {
        Ok(content) => content,
        Err(error) if is_not_exist(&error) => return Ok(result),
        Err(error) => return Err(error),
    };
    let Some(metadata) = plugin_yaml::decode(&content)
        .map_err(|error| anyhow!("plugin metadata {path}: {error}"))?
    else {
        // GUID-only metadata keeps the default importer: every platform.
        result.enabled = true;
        return Ok(result);
    };
    result.explicit = metadata.explicit != 0;
    result.constraints = metadata.constraints;
    let (mut any_enabled, mut explicit_editor, mut editor_enabled) = (false, false, false);
    for platform in &metadata.platforms {
        for (group, target) in &platform.first {
            if target == "Any" || group == "Any" {
                any_enabled = platform.enabled != 0;
                if platform
                    .settings
                    .get("Exclude Editor")
                    .is_some_and(|value| value == "1")
                {
                    any_enabled = false;
                }
            }
            if group == "Editor" && target == "Editor" {
                explicit_editor = true;
                editor_enabled = platform.enabled != 0;
            }
        }
    }
    // With "Any Platform" on, a platform is left out only by its exclusion;
    // its own entry counts when "Any Platform" is off.
    result.enabled = match any_enabled {
        true => true,
        false => explicit_editor && editor_enabled,
    };
    let constraints = match_constraints(&result.constraints, defines)?;
    result.enabled = result.enabled && constraints;
    Ok(result)
}
