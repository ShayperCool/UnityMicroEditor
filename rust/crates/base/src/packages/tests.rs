//! Resolver behaviour end to end: local, embedded, built-in, registry and Git packages
//! against temporary projects, loopback registries and temporary Git repositories.
use super::download::verify_integrity;
use super::files::{ensure_cache, inside, join, parent, safe_relative};
use super::git::{check_git_materialized, parse_git};
use super::lock::acquire_resolve_lock;
use super::semver::{compare_version, parse_version};
use super::*;
use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha512};
use std::cmp::Ordering as Order;
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

struct Temp(#[allow(dead_code)] tempfile::TempDir, PathBuf);
impl Temp {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = fs::canonicalize(dir.path()).unwrap();
        Self(dir, path)
    }
    fn path(&self, relative: &str) -> String {
        let path = if relative.is_empty() {
            self.1.clone()
        } else {
            self.1.join(relative)
        };
        path.to_str().unwrap().to_string()
    }
}
fn write(path: impl AsRef<Path>, data: &[u8]) {
    fs::create_dir_all(path.as_ref().parent().unwrap()).unwrap();
    fs::write(path, data).unwrap();
}
fn json_file(path: impl AsRef<Path>, value: &Value) {
    write(path, &serde_json::to_vec(value).unwrap());
}
fn manifest_path(root: &str) -> String {
    format!("{root}/Packages/manifest.json")
}
fn lock_path(root: &str) -> String {
    format!("{root}/Packages/packages-lock.json")
}
fn project_fixture(temp: &Temp, deps: Value) -> String {
    let root = temp.path("project");
    fs::create_dir_all(format!("{root}/Assets")).unwrap();
    json_file(manifest_path(&root), &json!({"dependencies": deps}));
    root
}
fn package_manifest(name: &str, version: &str, deps: Value) -> Value {
    json!({"name": name, "version": version, "dependencies": deps, "type": "", "unity": "", "unityRelease": ""})
}
fn package_fixture(path: &str, name: &str, version: &str, deps: Value) {
    json_file(
        format!("{path}/package.json"),
        &package_manifest(name, version, deps),
    );
    write(
        format!("{path}/Runtime/Probe.cs"),
        b"public class Probe {}\n",
    );
}
fn package_by_name<'a>(snapshot: &'a Snapshot, name: &str) -> &'a Package {
    snapshot
        .packages
        .iter()
        .find(|p| p.name == name)
        .unwrap_or_else(|| panic!("missing {name}"))
}
fn background() -> Context {
    Context::background()
}
fn offline() -> Options {
    Options {
        offline: true,
        ..Options::default()
    }
}
fn gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}
/// Appends one raw archive member; the name is written as given, unsafe or not.
fn member(archive: &mut tar::Builder<Vec<u8>>, name: &str, kind: u8, mode: u32, body: &[u8]) {
    let mut header = tar::Header::new_ustar();
    header.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
    header.set_entry_type(tar::EntryType::new(kind));
    header.set_mode(mode);
    header.set_size(body.len() as u64);
    header.set_cksum();
    archive.append(&header, body).unwrap();
}
/// One extended-header record: `<length> <key>=<value>\n`, the length counting itself.
fn pax_record(key: &str, value: &str) -> Vec<u8> {
    let rest = format!(" {key}={value}\n");
    let mut length = rest.len() + 1;
    while length != rest.len() + length.to_string().len() {
        length = rest.len() + length.to_string().len();
    }
    format!("{length}{rest}").into_bytes()
}
fn archive(files: &[(&str, &[u8])], special: Option<(&str, u8)>) -> Vec<u8> {
    let mut data = tar::Builder::new(Vec::new());
    for (name, body) in files {
        member(&mut data, name, b'0', 0o600, body);
    }
    if let Some((name, kind)) = special {
        member(&mut data, name, kind, 0o777, b"");
    }
    gzip(&data.into_inner().unwrap())
}

struct Request {
    path: String,
    headers: BTreeMap<String, String>,
}
struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}
impl Response {
    fn ok(body: Vec<u8>) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body,
        }
    }
    fn not_found() -> Self {
        Self {
            status: 404,
            headers: Vec::new(),
            body: b"404 page not found\n".to_vec(),
        }
    }
}
/// A minimal HTTP/1.1 server on a loopback port; returns its base URL.
fn serve(handler: impl Fn(&Request) -> Response + Send + Sync + 'static) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handler = Arc::new(handler);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let handler = handler.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                let path = line.split(' ').nth(1).unwrap_or("/").to_string();
                let mut headers = BTreeMap::new();
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 || header.trim().is_empty() {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':') {
                        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
                    }
                }
                let response = handler(&Request { path, headers });
                let mut head = format!(
                    "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                    response.status,
                    response.body.len()
                );
                for (name, value) in &response.headers {
                    head.push_str(&format!("{name}: {value}\r\n"));
                }
                head.push_str("\r\n");
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&response.body);
            });
        }
    });
    url
}
#[derive(Default)]
struct RegistryState {
    archives: BTreeMap<String, Vec<u8>>,
    versions: BTreeMap<String, BTreeMap<String, Value>>,
    wrong_integrity: bool,
    requests: usize,
}
struct Registry {
    url: String,
    state: Arc<Mutex<RegistryState>>,
}
impl Registry {
    fn new() -> Self {
        let state = Arc::new(Mutex::new(RegistryState::default()));
        let shared = state.clone();
        let url = Arc::new(Mutex::new(String::new()));
        let own_url = url.clone();
        let served = serve(move |request| {
            let mut state = shared.lock().unwrap();
            state.requests += 1;
            let key = request.path.trim_start_matches('/');
            if let Some(tarball) = key.strip_prefix("tar/") {
                return match state.archives.get(tarball) {
                    Some(data) => Response::ok(data.clone()),
                    None => Response::not_found(),
                };
            }
            let Some(versions) = state.versions.get(key) else {
                return Response::not_found();
            };
            let base = own_url.lock().unwrap().clone();
            let mut listed = serde_json::Map::new();
            for (version, dependencies) in versions {
                let mut sum = Sha512::digest(&state.archives[&format!("{key}@{version}")]).to_vec();
                if state.wrong_integrity {
                    sum[0] ^= 1;
                }
                let integrity = base64::engine::general_purpose::STANDARD.encode(sum);
                listed.insert(
                    version.clone(),
                    json!({"name": key, "version": version, "dependencies": dependencies,
                        "dist": {"tarball": format!("{base}/tar/{key}@{version}"), "integrity": format!("sha512-{integrity}"), "shasum": ""}}),
                );
            }
            let mut body = serde_json::to_vec(&json!({"name": key, "versions": listed})).unwrap();
            body.push(b'\n');
            Response::ok(body)
        });
        *url.lock().unwrap() = served.clone();
        Self { url: served, state }
    }
    fn add(&self, name: &str, version: &str, deps: Value) {
        let manifest = serde_json::to_vec(&package_manifest(name, version, deps.clone())).unwrap();
        let mut state = self.state.lock().unwrap();
        state
            .versions
            .entry(name.into())
            .or_default()
            .insert(version.into(), deps);
        state.archives.insert(
            format!("{name}@{version}"),
            archive(
                &[
                    ("package/package.json", &manifest),
                    ("package/Runtime/Source.cs", b"public class PackageProbe {}"),
                ],
                None,
            ),
        );
    }
    fn requests(&self) -> usize {
        self.state.lock().unwrap().requests
    }
}

#[test]
fn local_and_embedded_changes_invalidate_without_authored_writes() {
    let temp = Temp::new();
    let local = temp.path("local");
    package_fixture(&local, "com.example.local", "1.0.0", Value::Null);
    let root = project_fixture(
        &temp,
        json!({"com.example.local": format!("file:{local}"), "com.example.embedded": "9.0.0"}),
    );
    package_fixture(
        &format!("{root}/Packages/any-folder"),
        "com.example.embedded",
        "1.2.3",
        Value::Null,
    );
    let before = fs::read(manifest_path(&root)).unwrap();
    let s = resolve(&background(), &root, "", offline()).unwrap();
    assert!(s.packages.len() == 2 && s.lock_changed && !s.proposed_lock_path.is_empty());
    let embedded = package_by_name(&s, "com.example.embedded");
    assert!(embedded.source == "embedded" && embedded.version == "1.2.3");
    assert_eq!(before, fs::read(manifest_path(&root)).unwrap());
    assert!(
        !Path::new(&lock_path(&root)).exists(),
        "authored lock created"
    );
    write(
        format!("{local}/Runtime/Probe.cs"),
        b"public class Probe { public int changed; }",
    );
    assert!(
        s.validate(&background()).is_err(),
        "changed local C# ignored"
    );
    let next = resolve(&background(), &root, "", offline()).unwrap();
    assert_ne!(next.resolution_hash, s.resolution_hash);
    json_file(lock_path(&root), &json!({"dependencies": null}));
    assert!(
        next.validate(&background()).is_err(),
        "previously absent lock change ignored"
    );
}
#[test]
fn builtin_precedence_and_folder_independent_cache_identity() {
    let temp = Temp::new();
    let root = project_fixture(
        &temp,
        json!({"com.example.engine": "1.0.0", "com.example.cached": "2.0.0"}),
    );
    let editor = temp.path("editor/Unity");
    write(&editor, b"explicit editor profile");
    package_fixture(
        &temp.path("editor/Data/Resources/PackageManager/BuiltInPackages/arbitrary-folder"),
        "com.example.engine",
        "3.0.0",
        json!({"com.example.cached": "1.0.0"}),
    );
    package_fixture(
        &format!("{root}/Library/PackageCache/folder-does-not-prove-identity"),
        "com.example.cached",
        "2.0.0",
        Value::Null,
    );
    let s = resolve(&background(), &root, &editor, offline()).unwrap();
    let p = package_by_name(&s, "com.example.engine");
    assert!(p.source == "builtin" && p.version == "3.0.0" && p.proof == "engine_builtin");
    assert_eq!(package_by_name(&s, "com.example.cached").version, "2.0.0");
}
fn registry_project(temp: &Temp, registry: &Registry, extra: Value) -> String {
    let root = project_fixture(temp, Value::Null);
    let mut manifest =
        json!({"dependencies": {"com.example.a": "1.0.0"}, "registry": registry.url});
    if let Value::Object(extra) = extra {
        manifest.as_object_mut().unwrap().extend(extra);
    }
    json_file(manifest_path(&root), &manifest);
    root
}
#[test]
fn registry_closure_integrity_lock_proposal_and_offline_replay() {
    let f = Registry::new();
    f.add("com.example.a", "1.0.0", json!({"com.example.b": "1.0.0"}));
    f.add("com.example.b", "1.0.0", Value::Null);
    let temp = Temp::new();
    let root = registry_project(&temp, &f, Value::Null);
    let s = resolve(&background(), &root, "", Options::default()).unwrap();
    assert_eq!(s.packages.len(), 2);
    assert_eq!(package_by_name(&s, "com.example.b").depth, 1);
    assert_eq!(
        package_by_name(&s, "com.example.a").proof,
        "verified_registry_integrity"
    );
    assert!(
        !Path::new(&lock_path(&root)).exists(),
        "lock silently published"
    );
    let count = f.requests();
    let replay = resolve(&background(), &root, "", offline()).unwrap();
    assert!(replay.resolution_hash == s.resolution_hash && f.requests() == count);
    assert_eq!(replay, s);
    let current = load_current(&background(), &root, "", Options::default()).unwrap();
    assert_eq!(current, s);
    // The published snapshot is the indented encoding of the returned value.
    let mut expected = serde_json::to_vec_pretty(&s).unwrap();
    expected.push(b'\n');
    assert_eq!(fs::read(&s.snapshot_path).unwrap(), expected);
    assert_eq!(Snapshot::from_json(&expected).unwrap(), s);
    assert_eq!(
        fs::read(&s.proposed_lock_path).unwrap(),
        format!(
            r#"{{
  "dependencies": {{
    "com.example.a": {{
      "version": "1.0.0",
      "depth": 0,
      "source": "registry",
      "dependencies": {{
        "com.example.b": "1.0.0"
      }},
      "url": "{0}"
    }},
    "com.example.b": {{
      "version": "1.0.0",
      "depth": 1,
      "source": "registry",
      "dependencies": {{}},
      "url": "{0}"
    }}
  }}
}}
"#,
            f.url
        )
        .as_bytes()
    );
    write(
        format!(
            "{}/Runtime/Source.cs",
            package_by_name(&replay, "com.example.a").path
        ),
        b"tampered",
    );
    assert!(replay.validate(&background()).is_err());
    assert!(load_current(&background(), &root, "", Options::default()).is_err());
}
#[test]
fn registry_integrity_and_unsafe_archives_reject_before_snapshot() {
    for kind in ["integrity", "traversal", "symlink", "download", "expanded"] {
        let f = Registry::new();
        f.add("com.example.a", "1.0.0", Value::Null);
        {
            let mut state = f.state.lock().unwrap();
            state.wrong_integrity = kind == "integrity";
            if kind == "traversal" {
                state.archives.insert(
                    "com.example.a@1.0.0".into(),
                    archive(&[("../escape", b"bad")], None),
                );
            }
            if kind == "symlink" {
                state.archives.insert(
                    "com.example.a@1.0.0".into(),
                    archive(&[], Some(("package/link", b'2'))),
                );
            }
        }
        let temp = Temp::new();
        let root = registry_project(&temp, &f, Value::Null);
        let mut options = Options::default();
        if kind == "download" {
            options.max_download_bytes = 10;
        }
        if kind == "expanded" {
            options.max_expanded_bytes = 10;
        }
        let error = resolve(&background(), &root, "", options).expect_err(kind);
        let expected = match kind {
            "integrity" => "package archive integrity mismatch",
            "traversal" => "unsafe package archive path \"../escape\"",
            "symlink" => "package archive link/special entry unsupported: link",
            "download" => "package download exceeds byte bound",
            _ => "expanded package exceeds bounds",
        };
        assert_eq!(format!("{error:#}"), expected);
        let snapshots = format!("{root}/Library/UnityMicroEditor/packages/snapshots");
        assert!(
            fs::read_dir(snapshots).map_or(0, |entries| entries.count()) == 0,
            "failed package snapshot published"
        );
    }
}
#[test]
fn scoped_registry_uses_longest_namespace_and_rejects_ambiguity() {
    let (a, b) = (Registry::new(), Registry::new());
    b.add("com.example.tools.a", "1.0.0", Value::Null);
    let temp = Temp::new();
    let root = project_fixture(&temp, Value::Null);
    let manifest = |general: &str| {
        json!({"dependencies": {"com.example.tools.a": "1.0.0"}, "scopedRegistries": [
            {"name": "general", "url": a.url, "scopes": [general]},
            {"name": "specific", "url": b.url, "scopes": ["com.example.tools"]},
        ]})
    };
    json_file(manifest_path(&root), &manifest("com.example"));
    resolve(&background(), &root, "", Options::default()).unwrap();
    assert!(
        a.requests() == 0 && b.requests() != 0,
        "wrong registry selected"
    );
    json_file(manifest_path(&root), &manifest("com.example.tools"));
    let error = resolve(&background(), &root, "", Options::default()).unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        "ambiguous scoped registry for com.example.tools.a"
    );
}
#[test]
fn cycles_and_direct_version_conflicts_fail() {
    for cycle in [false, true] {
        let temp = Temp::new();
        let root = project_fixture(
            &temp,
            json!({"com.example.a": "1.0.0", "com.example.b": "1.0.0"}),
        );
        let dep = if cycle { "1.0.0" } else { "2.0.0" };
        package_fixture(
            &format!("{root}/Library/PackageCache/a"),
            "com.example.a",
            "1.0.0",
            json!({"com.example.b": dep}),
        );
        let deps = if cycle {
            json!({"com.example.a": "1.0.0"})
        } else {
            json!({})
        };
        package_fixture(
            &format!("{root}/Library/PackageCache/b"),
            "com.example.b",
            "1.0.0",
            deps,
        );
        let error = format!(
            "{:#}",
            resolve(&background(), &root, "", offline()).unwrap_err()
        );
        let expected = if cycle {
            "package dependency cycle: com.example.a -> com.example.b -> com.example.a"
        } else {
            "offline registry metadata unavailable for com.example.b"
        };
        assert_eq!(error, expected);
    }
}
#[test]
fn semver_ordering_and_unsupported_range() {
    let ordered = [
        "1.0.0-alpha",
        "1.0.0-alpha.1",
        "1.0.0-alpha.beta",
        "1.0.0-beta",
        "1.0.0-beta.2",
        "1.0.0-beta.11",
        "1.0.0-rc.1",
        "1.0.0",
        "1.0.1",
    ];
    for pair in ordered.windows(2) {
        assert_eq!(compare_version(pair[0], pair[1]), Order::Less, "{pair:?}");
        assert_eq!(
            compare_version(pair[1], pair[0]),
            Order::Greater,
            "{pair:?}"
        );
    }
    for invalid in ["^1.0.0", "1.0", "01.0.0", "1.0.0-01"] {
        assert!(parse_version(invalid).is_err(), "{invalid}");
    }
    assert_eq!(
        compare_version(
            "99999999999999999999999999999999.0.0",
            "100000000000000000000000000000000.0.0"
        ),
        Order::Less
    );
    assert_eq!(
        compare_version("1.0.0+build.1", "1.0.0+build.2"),
        Order::Equal
    );
    // Unparsable versions compare as the empty version.
    assert_eq!(compare_version("garbage", "0.0.0"), Order::Less);
    assert_eq!(compare_version("1.0.0-01", "1.0.0-1"), Order::Greater);
}
fn git(dir: &str, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}
fn commit(repo: &str, message: &str) {
    git(repo, &["add", "."]);
    git(
        repo,
        &[
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-m",
            message,
        ],
    );
}
#[test]
fn git_locked_commit_and_subdirectory_are_verified() {
    let temp = Temp::new();
    let repo = temp.path("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init"]);
    package_fixture(
        &format!("{repo}/Packages/Sub"),
        "com.example.git",
        "1.0.0",
        Value::Null,
    );
    commit(&repo, "one");
    let head = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["tag", "stable"]);
    let request = format!("git+file://{repo}?path=Packages/Sub#stable");
    let root = project_fixture(&temp, json!({"com.example.git": request}));
    json_file(
        lock_path(&root),
        &json!({"dependencies": {"com.example.git": {"version": request, "depth": 0, "source": "git", "dependencies": {}, "hash": head}}}),
    );
    write(
        format!("{repo}/Packages/Sub/Runtime/Probe.cs"),
        b"public class Newer {}",
    );
    commit(&repo, "two");
    git(&repo, &["tag", "-f", "stable"]);
    let s = resolve(&background(), &root, "", Options::default()).unwrap();
    let p = package_by_name(&s, "com.example.git").clone();
    assert!(
        p.revision == head && p.subdirectory == "Packages/Sub" && p.proof == "verified_git_commit",
        "{p:?}"
    );
    let data = fs::read_to_string(format!("{}/Runtime/Probe.cs", p.path)).unwrap();
    assert!(!data.contains("Newer"), "moved tag replaced locked commit");
    resolve(&background(), &root, "", offline()).unwrap();
    let cache = format!("{root}/Library/PackageCache/git-folder-name-is-not-evidence");
    package_fixture(&cache, "com.example.git", "1.0.0", Value::Null);
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(format!("{}/package.json", p.path)).unwrap()).unwrap();
    manifest["_fingerprint"] = json!("not-the-commit");
    json_file(format!("{cache}/package.json"), &manifest);
    let materialized = resolve(&background(), &root, "", offline()).unwrap();
    let copy = package_by_name(&materialized, "com.example.git");
    assert!(
        copy.path == cache
            && copy.commit_path == p.path
            && copy.commit_content_hash == p.content_hash
            && copy.revision == head
            && copy.proof == "verified_git_cache_copy",
        "{copy:?}"
    );
    load_current(&background(), &root, "", Options::default()).unwrap();
    write(
        format!("{cache}/Runtime/Probe.cs"),
        b"public class WrongCommit {}",
    );
    assert!(materialized.validate(&background()).is_err());
    let recovered = resolve(&background(), &root, "", offline()).unwrap();
    assert_eq!(package_by_name(&recovered, "com.example.git").path, p.path);
}
#[test]
fn registry_strategies_and_direct_minimum_upgrade() {
    for (strategy, minimum, expected) in [
        ("lowest", "1.2.0", "1.2.0"),
        ("highestPatch", "1.2.0", "1.2.7"),
        ("highestMinor", "1.2.0", "1.3.0"),
        ("highest", "1.2.0", "2.0.0"),
        ("highestMinor", "0.2.0", "0.2.7"),
    ] {
        let f = Registry::new();
        f.add(
            "com.example.root",
            "1.0.0",
            json!({"com.example.dep": minimum}),
        );
        for version in [
            "0.2.0",
            "0.2.7",
            "0.3.0",
            "1.2.0",
            "1.2.7",
            "1.3.0",
            "2.0.0",
            "3.0.0-preview.1",
        ] {
            f.add("com.example.dep", version, Value::Null);
        }
        let temp = Temp::new();
        let root = project_fixture(&temp, Value::Null);
        json_file(
            manifest_path(&root),
            &json!({"dependencies": {"com.example.root": "1.0.0"}, "registry": f.url, "resolutionStrategy": strategy}),
        );
        let s = resolve(&background(), &root, "", Options::default()).unwrap();
        assert_eq!(
            package_by_name(&s, "com.example.dep").version,
            expected,
            "{strategy} {minimum}"
        );
    }
    let f = Registry::new();
    f.add(
        "com.example.root",
        "1.0.0",
        json!({"com.example.dep": "1.3.0"}),
    );
    for version in ["1.2.0", "1.3.0", "2.0.0"] {
        f.add("com.example.dep", version, Value::Null);
    }
    let temp = Temp::new();
    let root = project_fixture(&temp, Value::Null);
    json_file(
        manifest_path(&root),
        &json!({"dependencies": {"com.example.root": "1.0.0", "com.example.dep": "1.2.0"}, "registry": f.url, "resolutionStrategy": "highest"}),
    );
    let s = resolve(&background(), &root, "", Options::default()).unwrap();
    assert_eq!(package_by_name(&s, "com.example.dep").version, "1.3.0");
}
#[test]
fn lock_preference_and_disabled_lock_preserve_authored_bytes() {
    let f = Registry::new();
    for version in ["1.0.0", "1.1.0", "2.0.0-preview.1"] {
        f.add("com.example.a", version, Value::Null);
    }
    for (lock_version, expected, enabled) in [
        ("1.1.0", "1.1.0", true),
        ("1.1.0", "1.0.0", false),
        ("2.0.0-preview.1", "1.0.0", true),
    ] {
        let temp = Temp::new();
        let root = registry_project(&temp, &f, json!({"enableLockFile": enabled}));
        json_file(
            lock_path(&root),
            &json!({"dependencies": {"com.example.a": {"version": lock_version, "depth": 0, "source": "registry", "dependencies": {}, "url": f.url}}}),
        );
        let before = fs::read(lock_path(&root)).unwrap();
        let s = resolve(&background(), &root, "", Options::default()).unwrap();
        assert_eq!(package_by_name(&s, "com.example.a").version, expected);
        assert_eq!(before, fs::read(lock_path(&root)).unwrap());
        assert!(enabled || s.lock_changed, "missing new proposal");
        // A lock that already matches the resolution is not proposed again.
        assert_eq!(s.lock_changed, !(enabled && lock_version == expected));
        assert_eq!(
            s.diagnostics.is_empty(),
            enabled,
            "enableLockFile diagnostic"
        );
    }
}
#[test]
fn embedded_addition_and_local_symlink_retarget_invalidate_current() {
    let temp = Temp::new();
    let root = project_fixture(&temp, Value::Null);
    let s = resolve(&background(), &root, "", offline()).unwrap();
    assert!(s.packages.is_empty() && !s.lock_changed);
    assert!(
        fs::read_to_string(&s.snapshot_path)
            .unwrap()
            .contains("\"packages\": []")
    );
    package_fixture(
        &format!("{root}/Packages/added-without-manifest-change"),
        "com.example.embedded",
        "1.0.0",
        Value::Null,
    );
    assert!(s.validate(&background()).is_err());
    let (first, second) = (temp.path("first"), temp.path("second"));
    package_fixture(&first, "com.example.local", "1.0.0", Value::Null);
    package_fixture(&second, "com.example.local", "1.0.0", Value::Null);
    let link = temp.path("selected");
    std::os::unix::fs::symlink(&first, &link).unwrap();
    json_file(
        manifest_path(&root),
        &json!({"dependencies": {"com.example.local": format!("file:{link}")}}),
    );
    let s = resolve(&background(), &root, "", offline()).unwrap();
    assert_eq!(package_by_name(&s, "com.example.local").path, first);
    fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&second, &link).unwrap();
    let error = s.validate(&background()).unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        "local package path changed: com.example.local"
    );
}
#[test]
fn cache_budget_and_unmaterialized_git_are_explicit_failures() {
    let f = Registry::new();
    f.add("com.example.a", "1.0.0", Value::Null);
    let temp = Temp::new();
    let root = registry_project(&temp, &f, Value::Null);
    let options = Options {
        max_cache_bytes: 32,
        ..Options::default()
    };
    let error = resolve(&background(), &root, "", options).unwrap_err();
    assert!(format!("{error:#}").contains("budget"), "{error:#}");
    for entry in ["large.fbx", ".gitmodules"] {
        let path = temp.path(&format!("lfs-{entry}/package"));
        write(
            format!("{path}/{entry}"),
            b"version https://git-lfs.github.com/spec/v1\noid sha256:test\nsize 1000\n",
        );
        assert!(check_git_materialized(&path).is_err(), "{entry}");
    }
}
#[test]
fn builtin_materialized_copy_requires_every_source_byte() {
    let temp = Temp::new();
    let root = project_fixture(&temp, json!({"com.example.engine": "1.0.0"}));
    let editor = temp.path("editor/Unity");
    write(&editor, b"explicit engine");
    let profile =
        temp.path("editor/Data/Resources/PackageManager/BuiltInPackages/com.example.engine");
    let cache = format!("{root}/Library/PackageCache/arbitrary-name");
    package_fixture(&profile, "com.example.engine", "1.0.0", Value::Null);
    package_fixture(&cache, "com.example.engine", "1.0.0", Value::Null);
    let mut value: Value =
        serde_json::from_slice(&fs::read(format!("{profile}/package.json")).unwrap()).unwrap();
    value["_fingerprint"] = json!("not-a-proof");
    json_file(format!("{cache}/package.json"), &value);
    let s = resolve(&background(), &root, &editor, offline()).unwrap();
    let p = package_by_name(&s, "com.example.engine");
    assert!(
        p.path == cache
            && p.profile_path == profile
            && p.content_hash != p.profile_content_hash
            && p.proof == "verified_engine_cache_copy",
        "{p:?}"
    );
    load_current(&background(), &root, &editor, Options::default()).unwrap();
    write(
        format!("{cache}/Runtime/Probe.cs"),
        b"public class Altered {}",
    );
    let error = resolve(&background(), &root, &editor, offline()).unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        "builtin PackageCache content differs from selected engine profile: com.example.engine"
    );
}
#[test]
fn concurrent_resolution_and_lock_cancellation() {
    let temp = Temp::new();
    let root = project_fixture(&temp, Value::Null);
    package_fixture(
        &format!("{root}/Packages/embedded"),
        "com.example.local",
        "1.0.0",
        Value::Null,
    );
    let hashes: Vec<String> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    resolve(&background(), &root, "", offline())
                        .unwrap()
                        .resolution_hash
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().unwrap()).collect()
    });
    assert!(hashes.iter().all(|hash| hash == &hashes[0]));
    load_current(&background(), &root, "", Options::default()).unwrap();
    let cache = ensure_cache(&root, "").unwrap();
    let _held = acquire_resolve_lock(&background(), &cache).unwrap();
    let ctx = background().with_timeout(std::time::Duration::from_millis(30));
    let error = resolve(&ctx, &root, "", offline()).unwrap_err();
    assert_eq!(
        error.downcast_ref::<ContextError>(),
        Some(&ContextError::DeadlineExceeded),
        "{error:#}"
    );
    let canceled = background();
    canceled.cancel();
    let error = resolve(&canceled, &root, "", offline()).unwrap_err();
    assert_eq!(
        error.downcast_ref::<ContextError>(),
        Some(&ContextError::Canceled)
    );
}

#[test]
fn registry_token_stays_inside_the_registry_namespace() {
    type Seen = Arc<Mutex<Vec<(String, String, String)>>>;
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let manifest =
        serde_json::to_vec(&package_manifest("com.example.a", "1.0.0", Value::Null)).unwrap();
    let tarball = archive(&[("package/package.json", &manifest)], None);
    let record = |seen: &Seen, host: &str, request: &Request| {
        seen.lock().unwrap().push((
            format!("{host}{}", request.path),
            request
                .headers
                .get("authorization")
                .cloned()
                .unwrap_or_default(),
            request.headers.get("referer").cloned().unwrap_or_default(),
        ));
    };
    let (seen_cdn, body) = (seen.clone(), tarball.clone());
    let cdn = serve(move |request| {
        record(&seen_cdn, "cdn", request);
        if request.path == "/blob" {
            Response::ok(body.clone())
        } else {
            Response::not_found()
        }
    });
    let registry_url = Arc::new(Mutex::new(String::new()));
    let (seen_registry, own, digest) = (
        seen.clone(),
        registry_url.clone(),
        base64::engine::general_purpose::STANDARD.encode(Sha512::digest(&tarball)),
    );
    let registry = serve(move |request| {
        record(&seen_registry, "registry", request);
        let base = own.lock().unwrap().clone();
        match request.path.as_str() {
            "/reg/com.example.a" => Response::ok(
                serde_json::to_vec(&json!({"name": "com.example.a", "versions": {"1.0.0": {
                    "dist": {"tarball": format!("{base}/reg/tar"), "integrity": format!("sha512-{digest}")}}}}))
                .unwrap(),
            ),
            "/reg/tar" => Response {
                status: 302,
                headers: vec![("Location".into(), "../hop".into())],
                body: Vec::new(),
            },
            "/hop" => Response {
                status: 307,
                headers: vec![("Location".into(), format!("{cdn}/blob"))],
                body: Vec::new(),
            },
            _ => Response::not_found(),
        }
    });
    *registry_url.lock().unwrap() = registry.clone();
    let temp = Temp::new();
    let root = project_fixture(&temp, Value::Null);
    json_file(
        manifest_path(&root),
        &json!({"dependencies": {"com.example.a": "1.0.0"}, "registry": format!("{registry}/reg/")}),
    );
    let options = Options {
        registry_tokens: BTreeMap::from([(format!("{registry}/reg"), "secret".to_string())]),
        ..Options::default()
    };
    let s = resolve(&background(), &root, "", options).unwrap();
    assert_eq!(
        package_by_name(&s, "com.example.a").registry,
        format!("{registry}/reg")
    );
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            (
                "registry/reg/com.example.a".to_string(),
                "Bearer secret".to_string(),
                String::new()
            ),
            (
                "registry/reg/tar".to_string(),
                "Bearer secret".to_string(),
                String::new()
            ),
            (
                "registry/hop".to_string(),
                String::new(),
                format!("{registry}/reg/tar")
            ),
            (
                "cdn/blob".to_string(),
                String::new(),
                format!("{registry}/hop")
            ),
        ]
    );
    // Redirect loops stop after ten requests, and failures name the registry host only.
    let looping = serve(|request| {
        if request.path == "/loop/tar" {
            Response {
                status: 302,
                headers: vec![("Location".into(), "/loop/tar".into())],
                body: Vec::new(),
            }
        } else if request.path == "/forbidden/com.example.a" {
            Response {
                status: 403,
                headers: Vec::new(),
                body: Vec::new(),
            }
        } else {
            Response::not_found()
        }
    });
    let host = looping.trim_start_matches("http://").to_string();
    for (registry, expected) in [
        ("missing", format!("package fetch HTTP 404 from {host}")),
        (
            "forbidden",
            format!("registry authentication required (403) for {host}"),
        ),
    ] {
        let temp = Temp::new();
        let root = project_fixture(&temp, Value::Null);
        json_file(
            manifest_path(&root),
            &json!({"dependencies": {"com.example.a": "1.0.0"}, "registry": format!("{looping}/{registry}")}),
        );
        let error = resolve(&background(), &root, "", Options::default()).unwrap_err();
        assert_eq!(format!("{error:#}"), expected);
    }
}
#[test]
fn git_requests_are_parsed_and_checked() {
    let reference =
        parse_git("git+https://Example.com/Org/Repo.git?path=/Packages/Sub#v1.0").unwrap();
    assert_eq!(
        (
            reference.remote.as_str(),
            reference.subdir.as_str(),
            reference.revision.as_str()
        ),
        ("https://Example.com/Org/Repo.git", "Packages/Sub", "v1.0")
    );
    let scp = parse_git("git@github.com:org/repo.git#main").unwrap();
    assert_eq!(
        (scp.remote.as_str(), scp.revision.as_str()),
        ("git@github.com:org/repo.git", "main")
    );
    assert_eq!(parse_git("file:///tmp/r").unwrap().remote, "file:///tmp/r");
    for (request, expected) in [
        (
            "git@github.com:org/repo.git?path=x",
            "SCP Git URLs with package path require ssh:// syntax",
        ),
        ("git@github.com", "invalid SCP Git URL"),
        (
            "git+ftp://host/repo.git",
            "unsupported Git protocol \"ftp\"",
        ),
        ("https:///repo.git", "Git URL host absent"),
        (
            "https://user:pw@host/repo.git",
            "Git credentials in URL unsupported; use credential configuration",
        ),
        (
            "https://host/repo.git?ref=x",
            "unsupported Git query \"ref\"",
        ),
        (
            "https://host/repo.git?path=../x",
            "unsafe Git package subdirectory",
        ),
        (
            "https://host/repo.git?path=a//b",
            "unsafe Git package subdirectory",
        ),
        (
            "https://host/repo.git?path=a:b",
            "unsafe Git package subdirectory",
        ),
        ("https://host/repo.git#-x", "unsafe Git revision"),
    ] {
        assert_eq!(
            parse_git(request).unwrap_err().to_string(),
            expected,
            "{request}"
        );
    }
    assert!(parse_git("ssh://git@host/repo.git").is_ok());
    for (value, expected) in [
        ("git+https://h/r", true),
        ("git://h/r", true),
        ("file:///r", true),
        ("ssh://h/r", true),
        ("git@h:r", true),
        ("https://h/r.git", true),
        ("https://h/r", false),
        ("file:../local", false),
        ("1.0.0", false),
    ] {
        assert_eq!(super::git::is_git(value), expected, "{value}");
    }
}
#[test]
fn integrity_uses_the_strongest_listed_digest() {
    let temp = Temp::new();
    let file = temp.path("blob");
    write(&file, b"payload");
    let b64 = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
    let sha512 = b64(&Sha512::digest(b"payload"));
    let sha256 = b64(&sha2::Sha256::digest(b"payload"));
    let sha1 = sha1::Sha1::digest(b"payload");
    let sha1_hex = crate::hex(&sha1);
    verify_integrity(&file, &format!("sha512-{sha512}"), "").unwrap();
    // The strongest listed algorithm decides, even when a weaker one would match.
    verify_integrity(&file, &format!("sha1-AAAA sha256-{sha256} md5-zz"), "").unwrap();
    assert!(
        verify_integrity(
            &file,
            &format!("sha256-{sha256} sha512-{}", b64(&[0; 64])),
            ""
        )
        .is_err()
    );
    verify_integrity(&file, "", &sha1_hex.to_uppercase()).unwrap();
    assert_eq!(
        verify_integrity(&file, "md5-AAAA nonsense", &sha1_hex)
            .unwrap_err()
            .to_string(),
        "unsupported registry integrity algorithm"
    );
    assert!(verify_integrity(&file, "", "zz").is_err());
    assert!(verify_integrity(&file, "sha512-***", "").is_err());
    assert_eq!(
        verify_integrity(&file, "", &crate::hex(&[0; 20]))
            .unwrap_err()
            .to_string(),
        "package archive integrity mismatch"
    );
}
#[test]
fn manifests_accept_null_fields_and_reject_malformed_input() {
    let temp = Temp::new();
    let root = temp.path("project");
    fs::create_dir_all(format!("{root}/Assets")).unwrap();
    write(
        manifest_path(&root),
        br#"{"dependencies":null,"scopedRegistries":null,"testables":null,"enableLockFile":null,"resolutionStrategy":null}"#,
    );
    write(
        format!("{root}/Packages/pkg/package.json"),
        br#"{"name":"com.example.embedded","version":"1.0.0","dependencies":null,"unity":"6000.0","extra":{"deep":[1,2,{"x":null}]}}"#,
    );
    let s = resolve(&background(), &root, "", offline()).unwrap();
    let p = package_by_name(&s, "com.example.embedded");
    assert!(p.version == "1.0.0" && p.unity == "6000.0" && p.dependencies.is_empty());
    assert!(s.diagnostics.is_empty());
    for (manifest, expected) in [
        (r#"{"dependencies":{"a":1}}"#, "expected a string"),
        (
            r#"{"scopedRegistries":[{"scopes":[1.5]}]}"#,
            "expected a string",
        ),
        (r#"{"dependencies":[]}"#, "expected a map"),
        (
            r#"{"resolutionStrategy":"newest"}"#,
            "unsupported resolutionStrategy \"newest\"",
        ),
        (
            r#"{"dependencies":{"Bad":"1.0.0"}}"#,
            "invalid project dependency \"Bad\"",
        ),
        (
            r#"{"dependencies":{"com.unity.modules.physics":"1.0.0"}}"#,
            "required engine module unavailable in selected Editor: com.unity.modules.physics",
        ),
        (
            r#"{"dependencies":{"com.example.x":"^1.0.0"}}"#,
            "unsupported Unity package version \"^1.0.0\": require exact SemVer",
        ),
        ("{", "EOF while parsing"),
    ] {
        write(manifest_path(&root), manifest.as_bytes());
        let error = format!(
            "{:#}",
            resolve(&background(), &root, "", offline()).unwrap_err()
        );
        assert!(error.contains(expected), "{manifest}: {error}");
    }
    assert!(resolve(&background(), &temp.path("missing"), "", offline()).is_err());
    let bounds = Options {
        max_files: 200001,
        ..Options::default()
    };
    write(manifest_path(&root), b"{}");
    assert_eq!(
        resolve(&background(), &root, "", bounds)
            .unwrap_err()
            .to_string(),
        "package resource bounds exceed supported validation limits"
    );
    let outside = Options {
        cache_root: temp.path("elsewhere"),
        ..Options::default()
    };
    assert_eq!(
        resolve(&background(), &root, "", outside)
            .unwrap_err()
            .to_string(),
        "package cache must be inside owned Library/UnityMicroEditor/packages"
    );
}
#[test]
fn local_tarball_and_file_modes() {
    let temp = Temp::new();
    let manifest =
        serde_json::to_vec(&package_manifest("com.example.tgz", "2.0.0", Value::Null)).unwrap();
    let mut data = tar::Builder::new(Vec::new());
    member(&mut data, "package/", b'5', 0o755, b"");
    member(&mut data, "package/package.json", b'0', 0o644, &manifest);
    member(
        &mut data,
        "package/Tools/run.sh",
        b'0',
        0o755,
        b"#!/bin/sh\n",
    );
    let root = project_fixture(&temp, json!({"com.example.tgz": "file:../pkg.tgz"}));
    write(
        format!("{root}/pkg.tgz"),
        &gzip(&data.into_inner().unwrap()),
    );
    let s = resolve(&background(), &root, "", offline()).unwrap();
    let p = package_by_name(&s, "com.example.tgz");
    assert!(
        p.source == "local"
            && p.proof == "verified_local_archive"
            && p.requested == "file:../pkg.tgz"
    );
    assert_eq!(
        p.path,
        format!(
            "{root}/Library/UnityMicroEditor/packages/content/{}",
            p.content_hash
        )
    );
    assert!(
        s.inputs.contains_key(&format!("{root}/pkg.tgz")),
        "{:?}",
        s.inputs
    );
    use std::os::unix::fs::PermissionsExt;
    let mode = |path: String| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(format!("{}/Tools/run.sh", p.path)) & 0o700, 0o700);
    assert_eq!(mode(format!("{}/package.json", p.path)) & 0o700, 0o600);
    assert_eq!(mode(s.snapshot_path.clone()), 0o600);
    load_current(&background(), &root, "", Options::default()).unwrap();
    // Replaying reuses the content-addressed directory.
    assert_eq!(resolve(&background(), &root, "", offline()).unwrap(), s);
}
#[test]
fn archive_metadata_members_are_applied_and_unsupported_members_refused() {
    let manifest =
        serde_json::to_vec(&package_manifest("com.example.tgz", "2.0.0", Value::Null)).unwrap();
    let unpack = |build: &dyn Fn(&mut tar::Builder<Vec<u8>>), cut: usize| {
        let temp = Temp::new();
        let root = project_fixture(&temp, json!({"com.example.tgz": "file:../pkg.tgz"}));
        let mut data = tar::Builder::new(Vec::new());
        member(&mut data, "package/package.json", b'0', 0o644, &manifest);
        build(&mut data);
        let data = data.into_inner().unwrap();
        write(format!("{root}/pkg.tgz"), &gzip(&data[..data.len() - cut]));
        let resolved = resolve(&background(), &root, "", offline())
            .map(|s| package_by_name(&s, "com.example.tgz").path.clone());
        (temp, resolved.map_err(|error| format!("{error:#}")))
    };
    let long = format!("package/{}/Deep.cs", "d".repeat(120));
    let older = format!("package/{}/Older.cs", "o".repeat(120));
    let (_temp, path) = unpack(
        &|data| {
            let comment = pax_record("comment", &"0".repeat(40));
            member(data, "pax_global_header", b'g', 0o666, &comment);
            let renamed = pax_record("path", "package/Runtime/Renamed.cs");
            member(data, "PaxHeaders/x", b'x', 0o644, &renamed);
            member(data, "package/short", b'0', 0o600, b"pax");
            let mut name = long.clone().into_bytes();
            name.push(0);
            member(data, "././@LongLink", b'L', 0, &name);
            member(data, "package/truncated", b'0', 0o600, b"gnu");
            // The same record under a header without a format mark, which
            // the archive reader passes through as an entry.
            let mut name = older.clone().into_bytes();
            name.push(0);
            let mut header = tar::Header::new_old();
            header.as_old_mut().name[..13].copy_from_slice(b"././@LongLink");
            header.set_entry_type(tar::EntryType::new(b'L'));
            header.set_mode(0);
            header.set_size(name.len() as u64);
            header.set_cksum();
            data.append(&header, &name[..]).unwrap();
            member(data, "package/cut", b'0', 0o600, b"old");
            member(data, "package/Old/", 0, 0o755, b"");
        },
        0,
    );
    let path = path.unwrap();
    let listed = |relative: &str| fs::read(format!("{path}/{relative}")).ok();
    assert_eq!(listed("Runtime/Renamed.cs").as_deref(), Some(&b"pax"[..]));
    assert_eq!(
        listed(long.strip_prefix("package/").unwrap()).as_deref(),
        Some(&b"gnu"[..])
    );
    assert_eq!(
        listed(older.strip_prefix("package/").unwrap()).as_deref(),
        Some(&b"old"[..])
    );
    assert!(listed("cut").is_none());
    assert!(Path::new(&format!("{path}/Old")).is_dir());
    assert!(listed("pax_global_header").is_none() && listed("short").is_none());
    let (_temp, sparse) = unpack(
        &|data| {
            let mut records = pax_record("GNU.sparse.major", "1");
            records.extend(pax_record("GNU.sparse.minor", "0"));
            member(data, "PaxHeaders/x", b'x', 0o644, &records);
            member(data, "package/sparse", b'0', 0o600, b"1\n0\n1\n");
        },
        0,
    );
    assert_eq!(
        sparse.unwrap_err(),
        "sparse package archive entry unsupported: package/sparse"
    );
    for (name, kind) in [
        ("package/old-sparse", b'S'),
        ("package/hard", b'1'),
        ("package/fifo", b'6'),
        ("package/device", b'3'),
    ] {
        let (_temp, refused) = unpack(&|data| member(data, name, kind, 0o600, b""), 0);
        assert!(refused.is_err(), "{name}");
    }
    let (_temp, duplicate) = unpack(
        &|data| member(data, "package/package.json", b'0', 0o644, &manifest),
        0,
    );
    assert_eq!(
        duplicate.unwrap_err(),
        "duplicate archive file package.json"
    );
    for name in ["package/a//b", "package/./a", "/absolute", "package/a:b"] {
        let (_temp, unsafe_path) = unpack(&|data| member(data, name, b'0', 0o600, b""), 0);
        let error = unsafe_path.unwrap_err();
        assert!(
            error.starts_with("unsafe package archive path"),
            "{name}: {error}"
        );
    }
    // A member cut short is a failure, not a shorter file.
    let (_temp, truncated) = unpack(
        &|data| member(data, "package/Big.bin", b'0', 0o600, &[7; 4096]),
        1024 + 2048,
    );
    assert!(truncated.is_err());
}
#[test]
fn git_package_at_repository_root_is_resolved() {
    let temp = Temp::new();
    let repo = temp.path("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init"]);
    package_fixture(&repo, "com.example.root", "1.0.0", Value::Null);
    commit(&repo, "one");
    let head = git(&repo, &["rev-parse", "HEAD"]);
    // Archiving a whole commit emits a `pax_global_header` member carrying the commit id.
    let listing = Command::new("git")
        .args(["-C", &repo, "archive", "--format=tar", &head])
        .output()
        .unwrap();
    assert!(
        listing
            .stdout
            .windows(17)
            .any(|window| window == b"pax_global_header")
    );
    let root = project_fixture(
        &temp,
        json!({"com.example.root": format!("git+file://{repo}")}),
    );
    let s = resolve(&background(), &root, "", Options::default()).unwrap();
    let p = package_by_name(&s, "com.example.root");
    assert!(
        p.source == "git"
            && p.revision == head
            && p.subdirectory.is_empty()
            && p.proof == "verified_git_commit",
        "{p:?}"
    );
    let mut files: Vec<_> = fs::read_dir(&p.path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    files.sort();
    assert_eq!(files, ["Runtime", "package.json"]);
    assert_eq!(resolve(&background(), &root, "", offline()).unwrap(), s);
    load_current(&background(), &root, "", Options::default()).unwrap();
}
#[test]
fn paths_are_joined_and_contained_lexically() {
    assert_eq!(join(&["/a", "", "../b"]), "/b");
    assert_eq!(join(&["/a/b", "./c//d/"]), "/a/b/c/d");
    assert_eq!(join(&["/a", "../../b"]), "/b");
    assert_eq!(join(&["a", "../.."]), "..");
    assert_eq!(join(&["", ""]), "");
    assert_eq!(
        (parent("/a/b"), parent("/Unity"), parent("")),
        ("/a".to_string(), "/".to_string(), String::new())
    );
    assert!(inside("/a", "/a") && inside("/a", "/a/b") && inside("/", "/a"));
    assert!(!inside("/a", "/ab") && !inside("/a/b", "/a") && !inside("/a", "/a/../b"));
    assert!(!inside("/a", "") && !inside("Data", "/a") && !inside("/a", "a"));
    for (path, safe) in [
        ("a/b.c", true),
        ("a", true),
        ("", false),
        (".", false),
        ("..", false),
        ("a/../b", false),
        ("a//b", false),
        ("/a", false),
        ("a/", false),
        ("a\\b", false),
        ("c:a", false),
    ] {
        assert_eq!(safe_relative(path.as_bytes()), safe, "{path:?}");
    }
}

#[test]
fn provided_dependency_resolves_without_a_manifest_declaration() {
    let temp = Temp::new();
    let root = project_fixture(&temp, json!({}));
    let tool = temp.path("tool/Packages/com.tool.runtime");
    package_fixture(&tool, "com.tool.runtime", "0.1.0", json!({}));
    let before = fs::read(manifest_path(&root)).unwrap();
    let provided = Options {
        provided: BTreeMap::from([("com.tool.runtime".to_string(), format!("file:{tool}"))]),
        ..offline()
    };
    let snapshot = resolve(&background(), &root, "", provided.clone()).unwrap();
    let package = package_by_name(&snapshot, "com.tool.runtime");
    assert_eq!(
        (package.source.as_str(), package.path.as_str()),
        ("local", fs::canonicalize(&tool).unwrap().to_str().unwrap())
    );
    assert_eq!(
        fs::read(manifest_path(&root)).unwrap(),
        before,
        "resolver wrote the provided package into the authored manifest"
    );
    assert!(
        fs::metadata(lock_path(&root)).is_err(),
        "resolver wrote an authored lock"
    );
    snapshot.validate(&background()).unwrap();

    // An authored declaration wins over the provided one.
    let declared = temp.path("declared");
    package_fixture(&declared, "com.tool.runtime", "0.2.0", json!({}));
    json_file(
        manifest_path(&root),
        &json!({"dependencies": {"com.tool.runtime": format!("file:{declared}")}}),
    );
    let snapshot = resolve(&background(), &root, "", provided).unwrap();
    assert_eq!(
        package_by_name(&snapshot, "com.tool.runtime").version,
        "0.2.0"
    );

    let invalid = Options {
        provided: BTreeMap::from([("com.tool.runtime".to_string(), "1.0.0".to_string())]),
        ..offline()
    };
    assert!(resolve(&background(), &root, "", invalid).is_err());
}

#[test]
fn downloaded_copy_wins_over_the_rewritten_package_cache_copy() {
    let f = Registry::new();
    f.add("com.example.a", "1.0.0", json!({"com.example.b": "1.0.0"}));
    f.add("com.example.b", "1.0.0", Value::Null);
    let temp = Temp::new();
    let root = registry_project(&temp, &f, Value::Null);
    let downloaded = resolve(&background(), &root, "", Options::default()).unwrap();
    // UPM's own copy of the same version, its package.json rewritten.
    let cache = format!("{root}/Library/PackageCache/com.example.a@0123456789ab");
    let mut manifest =
        package_manifest("com.example.a", "1.0.0", json!({"com.example.b": "1.0.0"}));
    manifest["_upm"] = json!({"changelog": ""});
    json_file(format!("{cache}/package.json"), &manifest);
    write(
        format!("{cache}/Runtime/Source.cs"),
        b"public class PackageProbe {}",
    );
    let s = resolve(&background(), &root, "", offline()).unwrap();
    assert_eq!(
        package_by_name(&s, "com.example.a").path,
        package_by_name(&downloaded, "com.example.a").path
    );
    // Without a downloaded copy, two differing UPM copies still conflict.
    let other = format!("{root}/Library/PackageCache/com.example.b@0123456789ab");
    package_fixture(&other, "com.example.b", "1.0.0", Value::Null);
    let second = format!("{root}/Library/PackageCache/com.example.b@ba9876543210");
    package_fixture(
        &second,
        "com.example.b",
        "1.0.0",
        json!({"com.example.a": "1.0.0"}),
    );
    fs::remove_dir_all(package_by_name(&downloaded, "com.example.b").path.clone()).unwrap();
    let error = format!(
        "{:#}",
        resolve(&background(), &root, "", offline()).unwrap_err()
    );
    assert!(
        error.contains("conflicting cached contents for com.example.b@1.0.0"),
        "{error}"
    );
}

#[test]
fn unreachable_git_package_takes_the_cache_copy_upm_fingerprinted() {
    let temp = Temp::new();
    let head = "0123456789abcdef0123456789abcdef01234567";
    let request = format!("git+file://{}#main", temp.path("missing-repository"));
    let root = project_fixture(&temp, json!({"com.example.git": request}));
    json_file(
        lock_path(&root),
        &json!({"dependencies": {"com.example.git": {"version": request, "depth": 0, "source": "git", "dependencies": {}, "hash": head}}}),
    );
    // A copy whose fingerprint names another commit is not taken.
    let stale = format!("{root}/Library/PackageCache/com.example.git@aaaaaaaaaaaa");
    let mut manifest = package_manifest("com.example.git", "1.0.0", Value::Null);
    manifest["_fingerprint"] = json!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    json_file(format!("{stale}/package.json"), &manifest);
    assert!(resolve(&background(), &root, "", Options::default()).is_err());
    let cache = format!("{root}/Library/PackageCache/com.example.git@012345678901");
    manifest["_fingerprint"] = json!(head);
    json_file(format!("{cache}/package.json"), &manifest);
    write(
        format!("{cache}/Runtime/Probe.cs"),
        b"public class Probe {}\n",
    );
    let s = resolve(&background(), &root, "", Options::default()).unwrap();
    let p = package_by_name(&s, "com.example.git");
    assert!(
        p.path == cache && p.revision == head && p.proof == "upm_fingerprinted_cache_copy",
        "{p:?}"
    );
    load_current(&background(), &root, "", Options::default()).unwrap();
    // Offline resolution still requires a verified commit.
    assert!(resolve(&background(), &root, "", offline()).is_err());
}

#[test]
fn runtime_package_dependencies_follow_the_release_line() {
    let temp = Temp::new();
    let root = project_fixture(&temp, json!({}));
    let version = |text: &str| {
        write(
            format!("{root}/ProjectSettings/ProjectVersion.txt"),
            text.as_bytes(),
        );
        runtime_dependencies(&root).map(|list| list.to_vec())
    };
    assert_eq!(
        version("m_EditorVersion: 6000.6.3f1\n").unwrap(),
        [
            ("com.unity.render-pipelines.core", "17.6.0"),
            ("com.unity.collections", "6.6.0")
        ]
    );
    assert_eq!(version("m_EditorVersion: 6000.0.71f1\n").unwrap(), []);
    assert!(version("m_EditorVersion: 2022.3.1f1\n").is_err());
    // A provided version yields to the project's own declaration.
    json_file(
        manifest_path(&root),
        &json!({"dependencies": {"com.example.local": format!("file:{}", temp.path("local"))}}),
    );
    package_fixture(
        &temp.path("local"),
        "com.example.local",
        "2.0.0",
        Value::Null,
    );
    let options = Options {
        provided_versions: BTreeMap::from([("com.example.local".to_string(), "1.0.0".to_string())]),
        ..offline()
    };
    let s = resolve(&background(), &root, "", options).unwrap();
    assert_eq!(package_by_name(&s, "com.example.local").version, "2.0.0");
}
