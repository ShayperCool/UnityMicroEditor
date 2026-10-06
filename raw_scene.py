"""Read raw Unity text assets without an Editor, Library or cooked bundles.

The initial loader deliberately accepts a small native component set. Unsupported
imports and dangling references are errors, not missing objects in a partial scene.
PyYAML is needed only for this command, not for the existing Player CLI.
"""
from __future__ import annotations

import copy
import hashlib
import json
from pathlib import Path
import re
from typing import Any

BUILTIN_GUIDS = {"0000000000000000e000000000000000", "0000000000000000f000000000000000"}
SUPPORTED_CLASSES = {1, 4, 20, 23, 33, 54, 65, 108, 114, 135, 136, 104,
                     64, 81, 82, 95, 120, 137, 143, 198, 199, 205, 222, 223, 224, 225,
                     29, 157, 196, 1660057539}
TRANSFORM_CLASSES = {4, 224}
TEXT_ASSETS = {".unity", ".prefab", ".mat", ".asset"}
NATIVE_MODEL_EXTENSIONS = {".fbx", ".obj", ".dae", ".blend", ".3ds", ".max", ".ma", ".mb"}
# Unity's serialized native files and importer-produced objects use distinct
# external reference kinds. Prefab references have special semantics and are
# intentionally outside this observed subset.
EXTERNAL_REFERENCE_TYPES = {".mat": 2, ".asset": 2, ".cs": 3,
                            ".png": 3, ".jpg": 3, ".jpeg": 3}
HEADER = re.compile(r"^--- !u!(\d+) &(-?\d+)(?: (stripped))?\s*$", re.MULTILINE)
GUID = re.compile(r"^guid:\s*([0-9a-fA-F]{32})\s*$", re.MULTILINE)
EDITOR_KEYS = {"m_CorrespondingSourceObject", "m_PrefabInstance", "m_PrefabAsset"}
MAX_TEXT_BYTES = 32 * 1024 * 1024


class RawSceneError(RuntimeError):
    def __init__(self, message: str, report: dict | None = None):
        super().__init__(message)
        self.report = report


class RawNativeClosureError(RawSceneError):
    def __init__(self, message, source):
        super().__init__(message)
        self.source = source


def _within(path: Path, root: Path) -> bool:
    parts, prefix = path.parts, root.parts
    return len(parts) >= len(prefix) and parts[:len(prefix)] == prefix


def _yaml(text: str) -> Any:
    try:
        import yaml
    except ImportError as exc:
        raise RawSceneError("Raw scene loading requires PyYAML. Install python3-yaml or run: python3 -m pip install PyYAML") from exc
    class Loader(getattr(yaml, "CSafeLoader", yaml.SafeLoader)):
        pass
    def mapping(loader, node, deep=False):
        result = {}
        for key_node, value_node in node.value:
            key = loader.construct_object(key_node, deep=deep)
            if not isinstance(key, (str, int, float, bool)) or key in result:
                raise RawSceneError(f"Duplicate or unsupported YAML key: {key!r}")
            result[key] = value_node.value if key in ("guid", "fileID") else loader.construct_object(value_node, deep=deep)
        return result
    Loader.yaml_implicit_resolvers = {key: [(tag, regexp) for tag, regexp in entries if tag not in {"tag:yaml.org,2002:bool", "tag:yaml.org,2002:timestamp"}]
                                      for key, entries in Loader.yaml_implicit_resolvers.items()}
    Loader.add_constructor(yaml.resolver.BaseResolver.DEFAULT_MAPPING_TAG, mapping)
    try:
        # Unity's file IDs are document anchors. Actual YAML aliases are not part
        # of the supported format and could turn a small file into a huge graph.
        if any(isinstance(token, yaml.tokens.AliasToken) for token in yaml.scan(text, Loader=Loader)):
            raise RawSceneError("YAML aliases are unsupported in raw Unity assets")
        value = yaml.load(text, Loader=Loader)
        return _normalise(value)
    except yaml.YAMLError as exc:
        raise RawSceneError(f"Invalid Unity YAML: {exc}") from exc


def _normalise(value):
    if isinstance(value, dict):
        result = {str(k): _normalise(v) for k, v in value.items()}
        if "fileID" in result:
            result["fileID"] = str(result["fileID"])
        if "guid" in result:
            result["guid"] = str(result["guid"]).lower()
        return result
    if isinstance(value, list):
        return [_normalise(v) for v in value]
    if isinstance(value, float):
        import math
        if not math.isfinite(value):
            raise RawSceneError("Non-finite floats are unsupported in raw Unity assets")
    return value


def _read_bytes(path: Path) -> bytes:
    with path.open("rb") as stream:
        content = stream.read(MAX_TEXT_BYTES + 1)
    if len(content) > MAX_TEXT_BYTES:
        raise RawSceneError(f"Text asset exceeds {MAX_TEXT_BYTES} bytes: {path}")
    return content


def _decode(content: bytes, path: Path) -> str:
    try:
        return content.decode("utf-8-sig")
    except UnicodeError as exc:
        raise RawSceneError(f"Binary serialized asset needs an importer: {path}") from exc


def _read(path: Path) -> str:
    return _decode(_read_bytes(path), path)


def parse_unity_yaml(path: Path, text: str | None = None) -> list[dict]:
    """Read Unity documents, preserving 64-bit object identities as strings."""
    text = _read(path) if text is None else text
    headers = list(HEADER.finditer(text))
    if not headers:
        raise RawSceneError(f"Not a Unity text serialized asset: {path}")
    result, seen = [], set()
    for index, header in enumerate(headers):
        file_id = header.group(2)
        if file_id in seen:
            raise RawSceneError(f"Duplicate fileID {file_id} in {path}")
        seen.add(file_id)
        end = headers[index + 1].start() if index + 1 < len(headers) else len(text)
        body = _yaml(text[header.end():end])
        if not isinstance(body, dict) or len(body) != 1:
            raise RawSceneError(f"Expected one Unity object at fileID {file_id}: {path}")
        type_name, data = next(iter(body.items()))
        if not isinstance(data, dict):
            raise RawSceneError(f"Expected object fields at fileID {file_id}: {path}")
        result.append({"classId": int(header.group(1)), "fileId": file_id,
                       "typeName": type_name, "data": data, "stripped": bool(header.group(3))})
    return result


def _reference_values(value, path=""):
    if isinstance(value, dict):
        if "fileID" in value:
            yield path, value
        else:
            for key, item in value.items():
                yield from _reference_values(item, f"{path}.{key}" if path else key)
    elif isinstance(value, list):
        for index, item in enumerate(value):
            yield from _reference_values(item, f"{path}[{index}]")


def _references(value, path=""):
    for property_path, reference in _reference_values(value, path):
        yield {"property": property_path, "fileId": str(reference["fileID"]),
               "guid": str(reference.get("guid", "")), "type": int(reference.get("type", 0))}


def _scalar_data(value):
    if isinstance(value, dict):
        if "fileID" in value:
            return None
        return {k: _scalar_data(v) for k, v in value.items()
                if not (isinstance(v, dict) and "fileID" in v)}
    if isinstance(value, list):
        return [_scalar_data(v) for v in value]
    return value


def _json(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"), allow_nan=False)


def _property(data, dotted):
    current = data
    for name in dotted.split("."):
        if not isinstance(current, dict) or name not in current:
            return None
        current = current[name]
    return current


def _equal(value, expected):
    if isinstance(value, (int, float)) and isinstance(expected, (int, float)):
        return abs(value - expected) <= 1e-6
    return value == expected


def _texture_dimensions(path):
    """Read dimensions only. Pixel decoding belongs to Unity's runtime importer."""
    import struct
    with path.open("rb") as stream:
        header = stream.read(24)
        if header.startswith(b"\x89PNG\r\n\x1a\n") and header[12:16] == b"IHDR":
            return struct.unpack(">II", header[16:24])
        if header[:2] != b"\xff\xd8":
            return None
        stream.seek(2)
        for _ in range(4096):
            if stream.read(1) != b"\xff":
                return None
            marker = stream.read(1)
            while marker == b"\xff":
                marker = stream.read(1)
            if not marker or marker in (b"\xd9", b"\xda"):
                return None
            if marker == b"\x01" or 0xD0 <= marker[0] <= 0xD7:
                continue
            length_bytes = stream.read(2)
            if len(length_bytes) != 2:
                return None
            length = struct.unpack(">H", length_bytes)[0]
            if length < 2:
                return None
            if marker[0] in {0xC0, 0xC1, 0xC2, 0xC3, 0xC5, 0xC6, 0xC7, 0xC9, 0xCA, 0xCB, 0xCD, 0xCE, 0xCF}:
                shape = stream.read(5)
                if len(shape) == 5:
                    height, width = struct.unpack(">HH", shape[1:])
                    return width, height
                return None
            stream.seek(length - 2, 1)
    return None


def _effective_texture_platform(settings):
    """Resolve legacy/default settings and enabled desktop overrides only."""
    effective = dict(settings)
    platforms = settings.get("platformSettings", [])
    for platform in platforms:
        if platform.get("buildTarget") == "DefaultTexturePlatform":
            effective.update(platform)
    for platform in platforms:
        if platform.get("buildTarget") == "Standalone" and int(platform.get("overridden", 0)):
            effective.update(platform)
    return effective


class _Reader:
    def __init__(self, project: Path, native_catalog=None, *, session_cache=None, source_inventory=None,
                 catalog_is_owned=False):
        self.project = project.resolve()
        self.session_cache = session_cache
        self._relative_cache = {}
        self.native_catalog_path = ""
        if isinstance(native_catalog, (str, Path)):
            catalog_path = Path(native_catalog).resolve()
            self.native_catalog_path = str(catalog_path)
            native_catalog = json.loads(catalog_path.read_text())
        # prepare_raw supplies a private validated catalog. Its large model
        # graph stays immutable; only models used by this scene are copied.
        self.native_catalog = (native_catalog or {}) if catalog_is_owned else copy.deepcopy(native_catalog or {})
        self.native_import_requirements = []
        if self.native_catalog and self.native_catalog.get("schemaVersion") != 1:
            raise RawSceneError("Native import catalog must declare schemaVersion 1")
        self.native_assets: dict[str, list[dict]] = {}
        self.native_models: dict[str, dict] = {}
        self.prefab_observations = {item["guid"]: item for item in self.native_catalog.get("prefabObservations", [])
                                    if item.get("complete") is True and isinstance(item.get("fileIds"), list)}
        self.ignored_prefab_overrides = {(item.get("path"), str(item.get("instanceFileId", "")), item.get("targetGuid"),
                                          str(item.get("targetFileId", "")), item.get("propertyPath"))
                                         for item in self.native_catalog.get("ignoredPrefabOverrides", [])
                                         if item.get("propertyExists") is False and isinstance(item.get("observedArraySize"), int)
                                         and item["observedArraySize"] >= 0}
        self.external_roots: dict[Path, str] = {}
        self.requires_scriptable_object_barrier = False
        for root in self.native_catalog.get("sourceRoots", []):
            logical = str(root.get("path", "")).strip("/")
            source = Path(root.get("source", "")).resolve()
            if not logical.startswith("Packages/") or ".." in Path(logical).parts or not source.is_dir():
                raise RawSceneError(f"Native catalog package root is invalid: {root}")
            if source in self.external_roots or logical in self.external_roots.values():
                raise RawSceneError(f"Duplicate native catalog package root: {logical}")
            if (self.project / logical).is_dir():
                # Embedded package content in the authored project takes
                # precedence over the import worker's copied package source.
                continue
            self.external_roots[source] = logical
        native_ids = set()
        for descriptor in self.native_catalog.get("assets", []):
            guid, file_id = str(descriptor.get("guid", "")).lower(), str(descriptor.get("fileId", ""))
            if not re.fullmatch(r"[0-9a-f]{32}", guid) or not re.fullmatch(r"-?\d+", file_id) or file_id == "0":
                raise RawSceneError(f"Invalid native catalog object identity: {descriptor}")
            if (guid, file_id) in native_ids:
                raise RawSceneError(f"Duplicate native catalog object: {guid}/{file_id}")
            native_ids.add((guid, file_id))
            self.native_assets.setdefault(guid, []).append(dict(descriptor, guid=guid, fileId=file_id, kind="imported"))
        for model in self.native_catalog.get("modelPrefabs", []):
            guid = str(model.get("guid", "")).lower()
            if not re.fullmatch(r"[0-9a-f]{32}", guid) or guid in self.native_models or not isinstance(model.get("objects"), list):
                raise RawSceneError(f"Invalid or duplicate native model graph: {guid}")
            self.native_models[guid] = model
        self.by_guid: dict[str, Path] = {}
        self.by_path: dict[Path, str] = {}
        self.diagnostics = []
        self.dependencies: dict[str, dict] = {}
        self.assets: dict[str, dict] = {}
        self.parsed = {}
        self.indexed_meta: dict[Path, bytes] = {}
        self.observed_text: dict[Path, bytes] = {}
        # Embedded/local package sources are supported. Imported Library package
        # caches must never be the source of truth for the raw project route.
        indexed_roots = [self.project / name for name in ("Assets", "Packages")]
        indexed_roots.extend(self.external_roots)
        library = (self.project / "Library").resolve()
        index_key = None
        if self.session_cache is not None:
            inventory = source_inventory if source_inventory is not None else self.session_cache.inventory(indexed_roots)
            index_key = self.session_cache.index_key(indexed_roots, inventory)
            cached = self.session_cache.cached_index(index_key)
            if cached is not None:
                by_guid, by_path, indexed_meta, diagnostics = cached
                self.by_guid.update(by_guid)
                self.by_path.update(by_path)
                self.indexed_meta.update(indexed_meta)
                self.diagnostics.extend(copy.deepcopy(diagnostics))
                return
        for root in indexed_roots:
            if not root.is_dir():
                continue
            for meta in sorted(root.rglob("*.meta")):
                resolved_meta = meta.resolve()
                if _within(resolved_meta, library):
                    self.error("project-library-input", meta, "Raw metadata cannot resolve inside the input project Library")
                    continue
                if not self._inside(meta, resolved_meta):
                    continue
                source = meta.with_suffix("")
                if not source.is_file():
                    continue
                content = self.read_bytes(meta, textual=True)[0]
                match = GUID.search(_decode(content, meta))
                if not match:
                    continue
                resolved_source = source.resolve()
                if _within(resolved_source, library):
                    self.error("project-library-input", source, "Raw source cannot resolve inside the input project Library")
                    continue
                if not self._inside(source, resolved_source):
                    self.error("external-symlink", source, "Raw assets must resolve inside the project")
                    continue
                guid = match.group(1).lower()
                if guid in self.by_guid:
                    self.error("duplicate-guid", source, f"GUID {guid} is also used by {self.relative(self.by_guid[guid])}")
                else:
                    self.by_guid[guid] = source
                    self.by_path[source] = guid
                    self.indexed_meta[meta] = content
        if index_key is not None:
            self.session_cache.store_index(index_key, self.by_guid, self.by_path, self.indexed_meta, self.diagnostics)

    def read_bytes(self, path, *, textual=False):
        if self.session_cache is not None:
            from raw_cache import RawCacheError
            try:
                return self.session_cache.read(path, maximum=MAX_TEXT_BYTES if textual else None)
            except RawCacheError as error:
                raise RawSceneError(str(error)) from error
        content = _read_bytes(path) if textual else path.read_bytes()
        return content, hashlib.sha256(content).hexdigest()

    def _inside(self, path, resolved=None):
        resolved = path.resolve() if resolved is None else resolved
        return _within(resolved, self.project) or any(_within(resolved, root) for root in self.external_roots)

    def relative(self, path):
        cached = self._relative_cache.get(path)
        if cached is not None:
            return cached
        try:
            result = path.relative_to(self.project).as_posix()
        except ValueError:
            resolved = path.resolve()
            for root, logical in self.external_roots.items():
                if _within(resolved, root):
                    result = logical + "/" + resolved.relative_to(root).as_posix()
                    break
            else:
                raise RawSceneError(f"Raw path is outside explicitly supplied source roots: {path}")
        self._relative_cache[path] = result
        return result

    def source(self, logical):
        path = self.project / logical
        if path.exists():
            return path
        for root, prefix in self.external_roots.items():
            if logical == prefix or logical.startswith(prefix + "/"):
                return root / logical[len(prefix):].lstrip("/")
        return path

    def error(self, code, path, message, file_id=""):
        self.diagnostics.append({"severity": "error", "code": code, "path": self.relative(path),
                                 "fileId": str(file_id), "message": message})

    def info(self, code, path, message, file_id="", severity="info"):
        self.diagnostics.append({"severity": severity, "code": code, "path": self.relative(path),
                                 "fileId": str(file_id), "message": message})

    def depend(self, path):
        for source in (path, Path(str(path) + ".meta")):
            if source.is_file():
                if _within(source.resolve(), (self.project / "Library").resolve()):
                    raise RawSceneError(f"Raw input cannot resolve inside the input project Library: {source}")
                name = self.relative(source)
                textual = source.suffix.lower() in TEXT_ASSETS | {".cs", ".meta", ".txt"}
                content, digest = self.read_bytes(source, textual=textual)
                observed = self.indexed_meta.get(source, self.observed_text.get(source))
                if observed is not None and observed != content:
                    raise RawSceneError(f"Raw source changed during GUID indexing or parsing; retry with stable sources: {source}")
                if textual:
                    self.observed_text[source] = content
                dependency = {"path": name, "sha256": digest, "bytes": len(content)}
                if not _within(source.resolve(), self.project):
                    dependency["sourcePath"] = str(source.resolve())
                self.dependencies[name] = dependency

    def text(self, path):
        if path not in self.observed_text:
            self.depend(path)
        return _decode(self.observed_text[path], path)

    def documents(self, path):
        self.depend(path)
        if path not in self.parsed:
            model = self.native_models.get(self.by_path.get(path, ""))
            if model is None and self.native_catalog and path.suffix.lower() in NATIVE_MODEL_EXTENSIONS:
                self.require_native_import(path, self.by_path.get(path, ""), "model")
                raise RawNativeClosureError(f"Native catalog lacks the imported model graph for {self.relative(path)}", path)
            if model is not None:
                ids = set()
                documents = []
                for document in copy.deepcopy(model["objects"]):
                    file_id = str(document.get("fileId", ""))
                    if not re.fullmatch(r"-?\d+", file_id) or file_id in ids or not isinstance(document.get("data"), dict):
                        raise RawSceneError(f"Invalid model graph object in {model.get('path')}: {file_id}")
                    ids.add(file_id)
                    documents.append(dict(document, fileId=file_id, classId=int(document["classId"]), stripped=False))
                native_ids = {entry["fileId"] for entry in self.native_assets.get(model["guid"], [])}
                for document in documents:
                    for _, reference in _reference_values(document["data"]):
                        target = str(reference["fileID"])
                        if not reference.get("guid") and target != "0" and target not in ids and target in native_ids:
                            # Imported model meshes, avatars and embedded
                            # materials are asset-local references. Hierarchy
                            # objects become scene-local; these native objects
                            # continue to resolve through the imported registry.
                            reference.update(guid=model["guid"], type=3)
                self.parsed[path] = documents
            else:
                text = self.text(path)
                if self.session_cache is not None:
                    self.parsed[path] = self.session_cache.documents(path, parse_unity_yaml, text)
                else:
                    self.parsed[path] = parse_unity_yaml(path, text)
            source_guid = self.by_path.get(path, "")
            observation = next((item for item in self.native_catalog.get("prefabObservations", [])
                                if item.get("guid") == source_guid and item.get("path") == self.relative(path)), None)
            if observation is not None:
                by_id = {document["fileId"]: document for document in self.parsed[path]}
                for missing in observation.get("missingComponents", []):
                    if missing.get("observedMissing") is not True or type(missing.get("componentIndex")) is not int:
                        continue
                    owner = by_id.get(str(missing.get("gameObjectFileId", "")))
                    index = missing["componentIndex"]
                    components = owner["data"].get("m_Component", []) if owner is not None and owner["classId"] == 1 else []
                    if not 0 <= index < len(components):
                        continue
                    component = by_id.get(str(components[index].get("component", {}).get("fileID", "0")))
                    if component is None or component["classId"] != 114:
                        continue
                    script_guid = component["data"].get("m_Script", {}).get("guid", "")
                    if not script_guid or script_guid in BUILTIN_GUIDS or script_guid in self.by_guid:
                        continue
                    component["_missingScript"] = True
                    self.info("missing-script-component", path,
                              f"Unity Editor observed a missing script at GameObject {owner['fileId']} component slot {index}; "
                              f"unresolved script {script_guid} is skipped. The public runtime API cannot reproduce GetComponents' null slot.",
                              component["fileId"], "warning")
            # Check the authored references before expansion rewrites prefab
            # aliases or replaces scalar/object-reference overrides.
            for document in self.parsed[path]:
                self.validate_reference_kinds(document["data"], path, document["fileId"])
        return copy.deepcopy(self.parsed[path])

    def require_native_import(self, path, guid, kind="asset"):
        requirement = {"path": self.relative(path), "guid": guid, "kind": kind}
        if requirement not in self.native_import_requirements:
            self.native_import_requirements.append(requirement)

    def validate_reference_kinds(self, data, path, file_id):
        for property_path, reference in _reference_values(data):
            guid = reference.get("guid", "")
            if "type" not in reference or not guid or guid in BUILTIN_GUIDS or str(reference["fileID"]) == "0":
                continue
            target = self.by_guid.get(guid)
            expected = EXTERNAL_REFERENCE_TYPES.get(target.suffix.lower()) if target is not None else None
            if expected is not None and str(reference["type"]) != str(expected):
                self.error("external-reference-type", path,
                           f"Reference {property_path} to {self.relative(target)} uses type {reference['type']!r}; "
                           f"expected type {expected} for {target.suffix.lower()} objects", file_id)

    def resolve(self, guid, path, file_id=""):
        source = self.by_guid.get(guid)
        if source is None:
            self.error("missing-guid", path, f"No raw asset .meta resolves GUID {guid}. Registry package sources must be supplied explicitly, not read from Library.", file_id)
        return source

    def expand(self, path, prefix="", stack=()):
        if path in stack:
            self.error("prefab-cycle", path, "Recursive prefab dependency: " + " -> ".join(self.relative(p) for p in (*stack, path)))
            return [], {}
        documents = self.documents(path)
        guid = self.by_path.get(path, "")
        own, children, mapping = [], [], {}
        for document in documents:
            if document["classId"] == 1001 or document["stripped"]:
                continue
            document.update(fileId=prefix + document["fileId"], sourcePath=self.relative(path), guid=guid,
                            _targets={(guid, document["fileId"])})
            mapping[document["fileId"][len(prefix):]] = document["fileId"]
            own.append(document)
        instances = []
        for instance in (d for d in documents if d["classId"] == 1001):
            data = instance["data"]
            source_guid = data.get("m_SourcePrefab", {}).get("guid", "")
            source = self.resolve(source_guid, path, instance["fileId"])
            if source is None:
                continue
            if source.suffix.lower() != ".prefab" and source_guid not in self.native_models:
                if self.native_catalog and source.suffix.lower() in NATIVE_MODEL_EXTENSIONS:
                    self.require_native_import(source, source_guid, "model")
                    raise RawNativeClosureError(f"Native catalog lacks the imported model graph for {self.relative(source)}", source)
                self.error("prefab-importer", path, f"Prefab source {self.relative(source)} requires an unsupported model/importer", instance["fileId"])
                self.depend(source)
                continue
            expanded, local_map = self.expand(source, prefix + instance["fileId"] + ":", (*stack, path))
            lookup = {target: obj for obj in expanded for target in obj["_targets"]}
            instances.append((instance, expanded, lookup, local_map, source_guid))
            children.extend(expanded)
        # Stripped scene entries alias source objects; callers can reference these
        # using their scene IDs, including objects from nested prefab variants.
        for stripped in (d for d in documents if d["stripped"]):
            data = stripped["data"]
            instance_id = str(data.get("m_PrefabInstance", {}).get("fileID", "0"))
            source_ref = data.get("m_CorrespondingSourceObject", {})
            instance = next((i for i in instances if i[0]["fileId"] == instance_id), None)
            target = instance[2].get((source_ref.get("guid", ""), str(source_ref.get("fileID", "0")))) if instance else None
            if target is None:
                self.error("stripped-object", path, "Cannot resolve stripped prefab object", stripped["fileId"])
            else:
                mapping[stripped["fileId"]] = target["fileId"]
                target["_targets"].add((guid, stripped["fileId"]))
        if path.suffix.lower() == ".prefab":
            # Unity gives inherited nested objects a stable asset-local ID even
            # when there is no stripped YAML document for that object. Those
            # IDs combine each nested instance ID with the original source ID.
            # The sign bit is cleared at the authored prefab asset boundary.
            # Existing stripped entries provide independently serialized checks
            # of this mapping, including imported models with negative IDs.
            for obj in children:
                relative_id = obj["fileId"][len(prefix):]
                identity = 0
                for part in relative_id.split(":"):
                    identity ^= int(part)
                local_id = str(identity & 0x7fffffffffffffff)
                previous = mapping.get(local_id)
                if previous is not None and previous != obj["fileId"]:
                    self.error("prefab-inherited-id", path, f"Inherited object ID {local_id} collides with a different authored object", local_id)
                    continue
                mapping[local_id] = obj["fileId"]
                obj["_targets"].add((guid, local_id))
        def rewrite(value):
            if isinstance(value, dict):
                if "fileID" in value:
                    result = dict(value)
                    original = str(value["fileID"])
                    if original != "0" and (not value.get("guid") or value.get("guid") == guid):
                        if original in mapping:
                            result["fileID"] = mapping[original]
                            result.pop("guid", None)
                            result.pop("type", None)
                    return result
                return {k: rewrite(v) for k, v in value.items() if k not in EDITOR_KEYS}
            if isinstance(value, list):
                return [rewrite(v) for v in value]
            return value
        for document in own:
            document["data"] = rewrite(document["data"])
        instance_roots = {}
        for instance, expanded, lookup, local_map, source_guid in instances:
            modification = instance["data"].get("m_Modification", {})
            ignored_targets = set()
            for override in modification.get("m_Modifications", []):
                target_ref = override.get("target", {})
                target = lookup.get((target_ref.get("guid", source_guid), str(target_ref.get("fileID", "0"))))
                if target is None:
                    target_guid = target_ref.get("guid", source_guid)
                    target_id = str(target_ref.get("fileID", "0"))
                    observed = self.prefab_observations.get(target_guid)
                    target_path = self.by_guid.get(target_guid)
                    if observed and target_path is not None and observed.get("path") == self.relative(target_path) and target_id not in {str(item) for item in observed["fileIds"]}:
                        key = (target_guid, target_id)
                        if key not in ignored_targets:
                            ignored_targets.add(key)
                            self.info("prefab-stale-target-override", path,
                                      f"Unity Editor observed no source object {target_guid}/{target_id}; retained overrides for this deleted object are ignored", instance["fileId"], "warning")
                        continue
                    self.error("prefab-override-target", path, f"Cannot resolve override target {target_ref}", instance["fileId"])
                    continue
                property_path = override.get("propertyPath", "")
                object_ref = override.get("objectReference", {"fileID": "0"})
                try:
                    self.apply_override(target["data"], property_path, override.get("value"), rewrite(object_ref))
                except (KeyError, ValueError, IndexError, TypeError) as exc:
                    proof_key = (self.relative(path), instance["fileId"], target_ref.get("guid", source_guid),
                                 str(target_ref.get("fileID", "0")), property_path)
                    if isinstance(exc, IndexError) and proof_key in self.ignored_prefab_overrides:
                        self.info("prefab-stale-array-override", path,
                                  f"Unity Editor found no property {property_path} after authored Array.size overrides; retained out-of-range override is ignored", instance["fileId"], "warning")
                        continue
                    self.error("prefab-override", path, f"Unsupported override {property_path}: {exc}", instance["fileId"])
            self.apply_structural_overrides(path, instance, expanded, lookup, own + children, rewrite, source_guid)
            parent = rewrite(modification.get("m_TransformParent", {"fileID": "0"}))
            roots = [o for o in expanded if o["classId"] in TRANSFORM_CLASSES and str(o["data"].get("m_Father", {}).get("fileID", "0")) == "0"]
            if len(roots) == 1:
                roots[0]["data"]["m_Father"] = parent
                instance_roots[instance["fileId"]] = roots[0]["fileId"]
            elif roots:
                self.error("prefab-roots", path, "Prefab must have exactly one root Transform", instance["fileId"])
        objects = [obj for obj in own + children if not obj.get("_removed")]
        for obj in own:
            if obj["classId"] == 1660057539:
                # Unity 6 SceneRoots can name a root PrefabInstance directly.
                # Its hierarchy is represented by the expanded root Transform.
                for reference in obj["data"].get("m_Roots", []):
                    original = str(reference.get("fileID", "0"))
                    if original in instance_roots:
                        reference["fileID"] = instance_roots[original]
        if path.suffix.lower() == ".prefab":
            # Unity 6000.6's PrefabImporter normalizes the imported root name
            # to the filename, even if the source YAML's m_Name differs.
            # Do this at the asset boundary: the caller then applies the
            # scene/nested instance overrides, including an authored m_Name.
            roots = [obj for obj in objects if obj["classId"] in TRANSFORM_CLASSES and str(obj["data"].get("m_Father", {}).get("fileID", "0")) == "0"]
            if len(roots) == 1:
                owner = str(roots[0]["data"].get("m_GameObject", {}).get("fileID", "0"))
                root_object = next((obj for obj in objects if obj["classId"] == 1 and obj["fileId"] == owner), None)
                if root_object is not None:
                    root_object["data"]["m_Name"] = path.stem
        return objects, mapping

    def apply_structural_overrides(self, path, instance, expanded, lookup, all_objects, rewrite, source_guid):
        """Apply authored prefab edits to the expanded source graph, never source files."""
        modification = instance["data"].get("m_Modification", {})
        by_id = {obj["fileId"]: obj for obj in all_objects}

        def source_target(reference):
            return lookup.get((reference.get("guid", source_guid), str(reference.get("fileID", "0"))))

        removed = set()
        for reference in modification.get("m_RemovedComponents", []):
            target = source_target(reference)
            if target is None or target["classId"] in {1, *TRANSFORM_CLASSES}:
                self.error("prefab-structural-override", path, f"Removed component is missing or is a GameObject/Transform: {reference}", instance["fileId"])
                continue
            removed.add(target["fileId"])
        for reference in modification.get("m_RemovedGameObjects", []):
            target = source_target(reference)
            if target is None or target["classId"] != 1:
                self.error("prefab-structural-override", path, f"Removed GameObject is missing or has a different type: {reference}", instance["fileId"])
                continue
            pending = [target["fileId"]]
            while pending:
                owner = pending.pop()
                if owner in removed:
                    continue
                removed.add(owner)
                components = [obj for obj in expanded if str(obj["data"].get("m_GameObject", {}).get("fileID", "0")) == owner]
                removed.update(obj["fileId"] for obj in components)
                transforms = {obj["fileId"] for obj in components if obj["classId"] in TRANSFORM_CLASSES}
                for child in expanded:
                    if child["classId"] in TRANSFORM_CLASSES and str(child["data"].get("m_Father", {}).get("fileID", "0")) in transforms:
                        pending.append(str(child["data"].get("m_GameObject", {}).get("fileID", "0")))

        def insert_unique(items, entry, key, index):
            reference = entry[key] if key else entry
            file_id = str(reference.get("fileID", "0"))
            items[:] = [item for item in items if str((item[key] if key else item).get("fileID", "0")) != file_id]
            if index == -1:
                items.append(entry)
            elif 0 <= index <= len(items):
                items.insert(index, entry)
            else:
                raise ValueError(f"Insertion index {index} is outside 0..{len(items)}")

        for field in ("m_AddedComponents", "m_AddedGameObjects"):
            for addition in modification.get(field, []):
                try:
                    target = source_target(addition["targetCorrespondingSourceObject"])
                    added_ref = rewrite(addition["addedObject"])
                    added = by_id.get(str(added_ref.get("fileID", "0")))
                    if target is None or added is None or target["fileId"] in removed:
                        raise ValueError("source target or authored addition is missing")
                    index = int(addition.get("insertIndex", -1))
                    if field == "m_AddedComponents":
                        if target["classId"] != 1 or added["classId"] in {1, *TRANSFORM_CLASSES}:
                            raise ValueError("added components require a GameObject target and a non-Transform component")
                        added["data"]["m_GameObject"] = {"fileID": target["fileId"]}
                        insert_unique(target["data"].setdefault("m_Component", []), {"component": {"fileID": added["fileId"]}}, "component", index)
                    else:
                        if target["classId"] not in TRANSFORM_CLASSES or added["classId"] not in TRANSFORM_CLASSES:
                            raise ValueError("added GameObjects are encoded as parent and child Transform references")
                        previous = by_id.get(str(added["data"].get("m_Father", {}).get("fileID", "0")))
                        if previous is not None and previous is not target:
                            previous["data"]["m_Children"] = [ref for ref in previous["data"].get("m_Children", []) if str(ref.get("fileID", "0")) != added["fileId"]]
                        added["data"]["m_Father"] = {"fileID": target["fileId"]}
                        insert_unique(target["data"].setdefault("m_Children", []), {"fileID": added["fileId"]}, "", index)
                except (KeyError, TypeError, ValueError) as exc:
                    self.error("prefab-structural-override", path, f"Invalid {field} entry {addition}: {exc}", instance["fileId"])

        for obj in all_objects:
            if obj["fileId"] in removed:
                obj["_removed"] = True
                continue
            data = obj["data"]
            if obj["classId"] == 1:
                data["m_Component"] = [entry for entry in data.get("m_Component", []) if str(entry.get("component", {}).get("fileID", "0")) not in removed]
            elif obj["classId"] in TRANSFORM_CLASSES:
                data["m_Children"] = [ref for ref in data.get("m_Children", []) if str(ref.get("fileID", "0")) not in removed]
            for _, reference in _reference_values(data):
                if not reference.get("guid") and str(reference.get("fileID", "0")) in removed:
                    reference.clear()
                    reference["fileID"] = "0"

    @staticmethod
    def apply_override(data, path, raw, object_ref):
        # Unity encodes an array index as .Array.data[N], not normal YAML keys.
        resizing = path.endswith(".Array.size")
        if resizing:
            path = path[:-len(".Array.size")]
        path = re.sub(r"\.Array\.data\[(\d+)\]", r"[\1]", path)
        tokens = re.findall(r"([^\.\[\]]+)|\[(\d+)\]", path)
        if not tokens:
            raise ValueError("empty property path")
        parts = [int(index) if index else key for key, index in tokens]
        array_element = any(isinstance(part, int) for part in parts)
        current = data
        for part in parts[:-1]:
            if isinstance(current, dict) and part not in current:
                # New serialized array elements can have fields which do not
                # exist in an empty source array. Only the explicit override
                # creates such fields; authored source fields are retained.
                if not array_element:
                    raise KeyError(part)
                current[part] = {}
            current = current[part]
        key = parts[-1]
        if resizing:
            items = current[key]
            if not isinstance(items, list):
                raise ValueError("Array.size must target a serialized array")
            size = int(raw)
            if size < 0 or size > 100000:
                raise ValueError("serialized array size must be in 0..100000")
            template = copy.deepcopy(items[-1]) if items else {}
            if size < len(items):
                del items[size:]
            else:
                items.extend(copy.deepcopy(template) for _ in range(size - len(items)))
            return
        if key == "m_RootOrder" and key not in current and "m_LocalPosition" in current:
            current[key] = int(raw)
            return
        if isinstance(current, dict) and key not in current and not array_element:
            raise KeyError(key)
        old = current.get(key) if isinstance(current, dict) else current[key]
        if isinstance(old, dict) and "fileID" in old:
            value = object_ref
        elif isinstance(old, bool):
            value = str(raw).lower() in ("1", "true")
        elif isinstance(old, int):
            text = str(raw)
            value = float(text) if any(character in text for character in ".eE") else int(text)
        elif isinstance(old, float):
            value = float(raw)
        elif old is None:
            if object_ref.get("guid") or str(object_ref.get("fileID", "0")) != "0":
                value = object_ref
            elif raw is None:
                value = ""
            else:
                text = str(raw)
                if re.fullmatch(r"-?\d+", text):
                    value = int(text)
                elif re.fullmatch(r"[-+]?(?:\d+\.\d*|\d*\.\d+|\d+)(?:[eE][-+]?\d+)?", text):
                    value = float(text)
                else:
                    value = text
        elif isinstance(old, str):
            value = "" if raw is None else str(raw)
        else:
            raise ValueError("only scalar and object-reference overrides are supported")
        current[key] = value

    def asset(self, guid, from_path, file_id=""):
        if not guid or guid in BUILTIN_GUIDS:
            return
        if guid in self.assets:
            return
        path = self.resolve(guid, from_path, file_id)
        if path is None:
            return
        self.depend(path)
        extension = path.suffix.lower()
        descriptor = {"guid": guid, "path": self.relative(path), "name": path.stem, "kind": "unsupported", "fileId": "0", "classId": 0,
                      "dataJson": "{}", "importerJson": "{}", "references": []}
        self.assets[guid] = descriptor  # Reserve first so cyclic data references terminate.
        meta = Path(str(path) + ".meta")
        importer = _yaml(self.text(meta)) if meta.is_file() else {}
        descriptor["importerJson"] = _json(importer)
        if extension == ".cs":
            descriptor["kind"] = "script"
            descriptor["fileId"] = "11500000"
            descriptor["executionOrder"] = int((importer or {}).get("MonoImporter", {}).get("executionOrder", 0))
            source = self.text(path)
            namespace = re.search(r"\bnamespace\s+([A-Za-z_][\w.]*)\s*[;{]", source)
            class_match = re.search(r"\bclass\s+" + re.escape(path.stem) + r"\b", source)
            if class_match:
                descriptor["typeName"] = (namespace.group(1) + "." if namespace else "") + path.stem
            else:
                self.error("script-type", path, "A referenced script must declare a class matching its filename")
        elif guid in self.native_assets:
            entries = []
            for native in self.native_assets[guid]:
                if native.get("path") != self.relative(path):
                    self.error("native-catalog-path", path, f"Native GUID {guid} catalog path {native.get('path')!r} differs from source {self.relative(path)!r}")
                entry = {"guid": guid, "path": self.relative(path), "name": native.get("name", path.stem),
                         "kind": "imported", "fileId": native["fileId"], "classId": int(native.get("classId", 0)),
                         "typeName": native.get("typeName", ""), "dataJson": "{}", "importerJson": "{}", "references": []}
                entries.append(entry)
            descriptor.update(kind="imported", _nativeObjects=entries)
        elif extension in (".png", ".jpg", ".jpeg"):
            descriptor.update(kind="texture", fileId="2800000", classId=28)
            settings = (importer or {}).get("TextureImporter", {})
            texture_type = int(settings.get("textureType", 0))
            if texture_type != 0 or int(settings.get("textureShape", 1)) != 1 or int(settings.get("convertToNormalMap", 0)):
                self.error("texture-importer", path, "Only ordinary 2D PNG/JPEG textures are supported; normal maps, sprites and cubemaps need additional importers")
            mipmap = settings.get("mipmaps", {})
            texture_settings = settings.get("textureSettings", {})
            clamp = lambda value, default: default if int(value) < 0 else int(value)
            descriptor["importerJson"] = _json({"textureType": texture_type, "textureShape": int(settings.get("textureShape", 1)),
                "sRGBTexture": int(mipmap.get("sRGBTexture", settings.get("sRGBTexture", 1))),
                "mipmapEnabled": int(mipmap.get("enableMipMap", mipmap.get("mipmapEnabled", 1))),
                "isReadable": int(settings.get("isReadable", 0)), "filterMode": clamp(texture_settings.get("filterMode", 1), 1),
                "aniso": clamp(texture_settings.get("aniso", 1), 1), "wrapU": clamp(texture_settings.get("wrapU", 0), 0),
                "wrapV": clamp(texture_settings.get("wrapV", 0), 0)})
            unsupported_settings = {"alphaIsTransparency": 0, "alphaUsage": 1, "grayScaleToAlpha": 0,
                "streamingMipmaps": 0, "vTOnly": 0, "ignoreMipmapLimit": 0, "applyGammaDecoding": 0,
                "ignorePngGamma": 0, "swizzle": 50462976, "flipbookRows": 1, "flipbookColumns": 1,
                "mipmaps.mipMapMode": 0, "mipmaps.fadeOut": 0, "mipmaps.borderMipMap": 0,
                "mipmaps.mipMapsPreserveCoverage": 0, "textureSettings.mipBias": 0,
                "bumpmap.convertToNormalMap": 0, "bumpmap.externalNormalMap": 0,
                "bumpmap.flipGreenChannel": 0}
            for property_path, expected in unsupported_settings.items():
                value = _property(settings, property_path)
                if value is not None and not _equal(value, expected):
                    self.error("texture-importer-property", path, f"TextureImporter.{property_path}={value!r} needs a raw importer adapter")
            dimensions = _texture_dimensions(path)
            effective_platform = _effective_texture_platform(settings)
            maximum = int(effective_platform.get("maxTextureSize", 0))
            if dimensions:
                descriptor.update(width=dimensions[0], height=dimensions[1])
                if maximum > 0 and max(dimensions) > maximum:
                    self.error("texture-resize", path, f"Raw {dimensions[0]}x{dimensions[1]} image exceeds maxTextureSize {maximum}; resizing is not implemented")
                if int(settings.get("nPOTScale", settings.get("npotScale", 0))) and any(size > 0 and size & (size - 1) for size in dimensions):
                    self.error("texture-npot-resize", path, "Non-power-of-two resizing is not implemented; use nPOTScale=0 to preserve source pixels")
            descriptor.update(sRGB=bool(settings.get("sRGBTexture", 1)),
                              mipmaps=bool(settings.get("mipmaps", {}).get("enableMipMap", 1)),
                              filterMode=int(settings.get("textureSettings", {}).get("filterMode", -1)),
                              wrapMode=int(settings.get("textureSettings", {}).get("wrapU", -1)))
            if "textureCompression" not in effective_platform:
                self.info("texture-runtime-format", path, "Effective desktop textureCompression is unspecified; runtime decoding assumes uncompressed pixels", severity="warning")
            elif int(effective_platform["textureCompression"]) != 0:
                self.error("texture-compression", path, "Effective desktop textureCompression is not Uncompressed (0); native texture compression is not implemented")
            if int(effective_platform.get("crunchedCompression", 0)):
                self.error("texture-crunch", path, "Effective desktop crunchedCompression is enabled; Crunch encoding is not implemented")
            if int(effective_platform.get("textureFormat", -1)) != -1:
                self.error("texture-format", path, "Effective desktop textureFormat is explicit; the raw importer supports only Automatic (-1) with uncompressed RGB24/RGBA32 pixels")
        elif extension == ".mat":
            docs = self.documents(path)
            if len(docs) != 1 or docs[0]["classId"] != 21:
                self.error("material-format", path, "Expected one Material object")
                return
            data = docs[0]["data"]
            descriptor["name"] = data.get("m_Name") or path.stem
            descriptor.update(kind="material", fileId=docs[0]["fileId"], classId=21, dataJson=_json(data), references=list(_references(data)))
            shader = data.get("m_Shader", {})
            if shader.get("guid") in BUILTIN_GUIDS and str(shader.get("fileID")) == "46":
                descriptor["shaderName"] = "Standard"
            else:
                self.error("shader-importer", path, f"Shader {shader} has no raw runtime compiler; initial host supports the builtin Standard shader")
            for field in ("m_ValidKeywords", "m_InvalidKeywords", "m_ShaderKeywords", "disabledShaderPasses", "stringTagMap", "m_ModifiedSerializedProperties", "m_BuildTextureStacks"):
                if data.get(field):
                    self.error("material-keywords", path, f"Material {field} is not supported by the raw host")
            if str(data.get("m_Parent", {}).get("fileID", "0")) != "0":
                self.error("material-variant", path, "Material variants require parent-property inheritance, which is not implemented")
            if int(data.get("m_CustomRenderQueue", -1)) != -1 or data.get("m_EnableInstancingVariants", 0):
                self.error("material-render-settings", path, "Custom render queue and material instancing need runtime adapters")
            properties = data.get("m_SavedProperties", {})
            colors = {k: v for entry in properties.get("m_Colors", []) for k, v in entry.items()}
            floats = {k: v for entry in properties.get("m_Floats", []) for k, v in entry.items()}
            textures = {k: v for entry in properties.get("m_TexEnvs", []) for k, v in entry.items()}
            if properties.get("m_Ints"):
                self.error("material-int-property", path, "Material integer properties need explicit runtime adapters")
            defaults = {"_Mode": 0, "_Metallic": 0, "_Glossiness": 0.5, "_GlossMapScale": 1, "_SmoothnessTextureChannel": 0,
                        "_BumpScale": 1, "_Parallax": 0.02, "_OcclusionStrength": 1, "_Cutoff": 0.5, "_UVSec": 0,
                        "_SrcBlend": 1, "_DstBlend": 0, "_ZWrite": 1}
            for name, value in floats.items():
                if name not in defaults or abs(float(value) - defaults[name]) > 1e-6:
                    self.error("material-float-property", path, f"Material property {name}={value} needs a runtime adapter")
            for name, value in colors.items():
                if name != "_Color" and not (name == "_EmissionColor" and all(float(value.get(k, 0)) == 0 for k in ("r", "g", "b"))):
                    self.error("material-color-property", path, f"Material color property {name} is not supported")
            texture = textures.get("_MainTex", {})
            descriptor.update(color=colors.get("_Color", {"r": 1, "g": 1, "b": 1, "a": 1}),
                              textureGuid=texture.get("m_Texture", {}).get("guid", ""),
                              textureFileId=str(texture.get("m_Texture", {}).get("fileID", "0")),
                              textureScale=texture.get("m_Scale", {"x": 1, "y": 1}),
                              textureOffset=texture.get("m_Offset", {"x": 0, "y": 0}), materialMode=int(floats.get("_Mode", 0)))
            if descriptor["materialMode"] != 0:
                self.error("material-mode", path, "Transparent/cutout Standard materials are not supported by this initial host")
            for name, value in textures.items():
                if name != "_MainTex" and str(value.get("m_Texture", {}).get("fileID", "0")) != "0":
                    self.error("material-texture-property", path, f"Material texture property {name} is not supported")
            for reference in descriptor["references"]:
                self.asset(reference["guid"], path, reference["fileId"])
        elif extension == ".prefab":
            descriptor["kind"] = "prefab"
            objects, aliases = self.expand(path)
            self.validate_objects(objects)
            roots = [obj for obj in objects if obj["classId"] in TRANSFORM_CLASSES and str(obj["data"].get("m_Father", {}).get("fileID", "0")) == "0"]
            if len(roots) != 1:
                self.error("runtime-prefab-roots", path, f"Runtime prefab needs exactly one root Transform, found {len(roots)}", file_id)
            else:
                owner = str(roots[0]["data"].get("m_GameObject", {}).get("fileID", "0"))
                descriptor.update(fileId=owner, classId=1, rootFileId=owner,
                                  objectAliases=[{"fileId": local_id, "objectId": expanded_id} for local_id, expanded_id in sorted(aliases.items())],
                                  objects=[self.manifest_object(obj) for obj in objects])
        elif extension == ".asset":
            docs = self.documents(path)
            if len(docs) != 1 or docs[0]["classId"] != 114:
                if self.native_catalog:
                    self.require_native_import(path, guid)
                self.error("native-import-required" if self.native_catalog else "scriptable-object", path,
                           "Only a single custom ScriptableObject class114 is supported; native .asset importers need adapters")
            else:
                data = docs[0]["data"]
                descriptor["name"] = data.get("m_Name") or path.stem
                def local_refs(value):
                    if isinstance(value, dict):
                        if "fileID" in value and value["fileID"] != "0" and not value.get("guid"):
                            if value["fileID"] != docs[0]["fileId"]:
                                self.error("asset-local-reference", path, f"Unknown local ScriptableObject fileID {value['fileID']}")
                            else:
                                value["guid"] = guid
                        else:
                            for item in value.values():
                                local_refs(item)
                    elif isinstance(value, list):
                        for item in value:
                            local_refs(item)
                local_refs(data)
                descriptor.update(kind="scriptableObject", classId=114, fileId=docs[0]["fileId"], dataJson=_json(data), references=list(_references(data)))
                script_guid = data.get("m_Script", {}).get("guid", "")
                self.asset(script_guid, path, docs[0]["fileId"])
                descriptor["typeName"] = self.assets.get(script_guid, {}).get("typeName", "")
                if not descriptor["typeName"]:
                    self.error("scriptable-object-type", path, "ScriptableObject needs a resolvable raw script type")
                script_source = self.by_guid.get(script_guid)
                if script_source is not None:
                    code = re.sub(r"/\*.*?\*/|//[^\n]*", "", self.text(script_source), flags=re.DOTALL)
                    if re.search(r"\bvoid\s+(Awake|OnEnable)\s*\(", code) and not self.native_catalog:
                        self.error("scriptable-object-callback", path, "CreateInstance invokes Awake/OnEnable before raw fields can be assigned; callback-bearing ScriptableObjects are unsupported")
                    declaration = re.search(r"\bclass\s+" + re.escape(script_source.stem) + r"\s*:\s*([^\{]+)", code)
                    if not self.native_catalog and (not declaration or not any(base.strip() in ("ScriptableObject", "UnityEngine.ScriptableObject") for base in declaration.group(1).split(","))):
                        self.error("scriptable-object-base", path, "Initial raw ScriptableObject support requires a direct ScriptableObject base so inherited lifecycle callbacks cannot be missed")
                if self.native_catalog:
                    self.requires_scriptable_object_barrier = True
            for doc in docs:
                for reference in _references(doc["data"]):
                    self.asset(reference["guid"], path, reference["fileId"])
        else:
            from imported_assets import NATIVE_EXTENSIONS
            requires_native = bool(self.native_catalog and extension in NATIVE_EXTENSIONS)
            if requires_native:
                self.require_native_import(path, guid)
            self.error("native-import-required" if requires_native else "asset-importer", path,
                       f"Raw {extension or 'extensionless'} import is not implemented")

    def validate_native_settings(self, obj):
        data, class_id = obj["data"], obj["classId"]
        path = self.source(obj["sourcePath"])
        defaults = {}
        refs = []
        if class_id == 1:
            defaults = {"m_StaticEditorFlags": 0}
            tag = data.get("m_TagString", "Untagged")
            if tag not in {"Untagged", "Respawn", "Finish", "EditorOnly", "MainCamera", "Player", "GameController"}:
                self.error("custom-tag", path, f"Custom tag {tag!r} must be registered in a prepared host; raw TagManager import is not implemented", obj["fileId"])
        elif class_id in TRANSFORM_CLASSES:
            rotation = data.get("m_LocalRotation")
            if isinstance(rotation, dict):
                components = [float(rotation.get(axis, default)) for axis, default in (("x", 0), ("y", 0), ("z", 0), ("w", 1))]
                norm_squared = sum(value * value for value in components)
                if abs(norm_squared - 1.0) > 1e-5:
                    self.error("transform-quaternion", path,
                               f"Transform.m_LocalRotation has squared norm {norm_squared!r}; "
                               "non-unit serialized quaternions are unsupported because the public "
                               "Transform.localRotation setter normalizes them", obj["fileId"])
        elif class_id == 20:
            defaults = {"m_RenderingPath": -1, "m_TargetEye": 3, "m_AllowDynamicResolution": 0, "m_ForceIntoRT": 0,
                        "m_OcclusionCulling": 1, "m_LensShift.x": 0, "m_LensShift.y": 0,
                        "m_StereoConvergence": 10, "m_StereoSeparation": 0.022}
            refs = []
            mode = data.get("m_projectionMatrixMode", data.get("ProjectionMatrixMode", 1))
            if mode not in (0, 1):
                self.error("physical-camera", path, f"Projection mode {mode} requires a physical-camera importer", obj["fileId"])
        elif class_id == 23:
            defaults = {"m_DynamicOccludee": 1, "m_StaticShadowCaster": 0, "m_MotionVectors": 1,
                        "m_RenderingLayerMask": 1, "m_RendererPriority": 0, "m_SortingLayerID": 0,
                        "m_SortingLayer": 0, "m_SortingOrder": 0, "m_ForceMeshLod": -1,
                        "m_MeshLodSelectionBias": 0, "m_StaticBatchInfo.firstSubMesh": 0,
                        "m_StaticBatchInfo.subMeshCount": 0}
            refs = ["m_StaticBatchRoot", "m_ProbeAnchor", "m_LightProbeVolumeOverride", "m_LightmapParameters"]
            index = int(data.get("m_LightmapIndex", 65535))
            if 0 <= index < 65534:
                self.error("baked-renderer", path, "Renderer has a baked lightmap index", obj["fileId"])
        elif class_id == 54:
            defaults = {"m_ImplicitCom": 1, "m_ImplicitTensor": 1, "m_IncludeLayers.m_Bits": 0,
                        "m_ExcludeLayers.m_Bits": 0, "m_LayerOverridePriority": 0,
                        "m_SolverIterations": 6, "m_SolverVelocityIterations": 1}
        elif class_id in (65, 135, 136):
            defaults = {"m_IncludeLayers.m_Bits": 0, "m_ExcludeLayers.m_Bits": 0,
                        "m_LayerOverridePriority": 0, "m_ProvidesContacts": 0, "m_ContactOffset": 0.01}
            refs = ["m_Material"]
        elif class_id == 108:
            defaults = {"m_DrawHalo": 0, "m_RenderMode": 0, "m_RenderingLayerMask": 1,
                        "m_Lightmapping": 4,
                        "m_UseBoundingSphereOverride": 0, "m_UseViewFrustumForShadowCasterCull": 1,
                        "m_ForceVisible": 0, "m_ShapeRadius": 0, "m_ShadowAngle": 0,
                        "m_Shadows.m_Resolution": -1, "m_Shadows.m_CustomResolution": -1,
                        "m_Shadows.m_Bias": 0.05, "m_Shadows.m_NormalBias": 0.4,
                        "m_Shadows.m_NearPlane": 0.2, "m_Shadows.m_UseCullingMatrixOverride": 0}
            refs = ["m_Cookie", "m_Flare"]
            if int(data.get("m_Type", 1)) not in (0, 1, 2):
                self.error("light-type", path, "Only realtime spot, directional and point lights have adapters", obj["fileId"])
        elif class_id == 104:
            defaults = {"m_DefaultReflectionMode": 0, "m_DefaultReflectionResolution": 128,
                        "m_ReflectionBounces": 1, "m_UseRadianceAmbientProbe": 0}
            refs = ["m_CustomReflection", "m_HaloTexture"]
        for property_path, expected in defaults.items():
            value = _property(data, property_path)
            if value is not None and not _equal(value, expected):
                self.error("native-property", path, f"{obj['typeName']}.{property_path}={value!r} needs an adapter; initial host accepts only {expected!r}", obj["fileId"])
        for property_path in refs:
            value = _property(data, property_path)
            if value is not None and str(value.get("fileID", "0")) != "0":
                self.error("native-reference-property", path, f"{obj['typeName']}.{property_path} is not applied by the initial native adapter", obj["fileId"])

    def order_scene_roots(self, objects, path):
        roots = [obj for obj in objects if obj["classId"] in TRANSFORM_CLASSES and str(obj["data"].get("m_Father", {}).get("fileID", "0")) == "0"]
        metadata = [obj for obj in objects if obj["classId"] == 1660057539]
        if len(metadata) > 1:
            self.error("scene-roots", path, "Expected at most one SceneRoots document")
        if metadata:
            root_ids = [str(ref.get("fileID", "0")) for ref in metadata[0]["data"].get("m_Roots", [])]
            if len(set(root_ids)) != len(root_ids) or set(root_ids) != {obj["fileId"] for obj in roots}:
                self.error("scene-roots", path, "SceneRoots must list every root Transform exactly once")
                return objects
        elif any("m_RootOrder" in obj["data"] for obj in roots):
            ordered = sorted(enumerate(roots), key=lambda pair: (int(pair[1]["data"].get("m_RootOrder", pair[0])), pair[0]))
            root_ids = [obj["fileId"] for _, obj in ordered]
        else:
            return objects
        rank = {file_id: index for index, file_id in enumerate(root_ids)}
        owner_rank = {str(obj["data"].get("m_GameObject", {}).get("fileID", "0")): rank[obj["fileId"]] for obj in roots}
        def key(pair):
            index, obj = pair
            if obj["classId"] == 1 and obj["fileId"] in owner_rank:
                return 0, owner_rank[obj["fileId"]], 0
            if obj["classId"] in TRANSFORM_CLASSES and obj["fileId"] in rank:
                return 0, rank[obj["fileId"]], 1
            return 1, index, 0
        return [obj for _, obj in sorted(enumerate(objects), key=key)]

    def manifest_object(self, obj):
        data = obj["data"]
        if obj["classId"] == 20:
            data = {({"near clip plane": "nearClipPlane", "far clip plane": "farClipPlane", "field of view": "fieldOfView", "orthographic size": "orthographicSize", "m_projectionMatrixMode": "ProjectionMatrixMode"}.get(key, key)): value for key, value in data.items()}
        type_name = obj["typeName"]
        if obj["classId"] == 114:
            script = data.get("m_Script", {})
            script_guid = script.get("guid", "")
            type_name = self.assets.get(script_guid, {}).get("typeName", "")
            if script_guid == "0000000000000000e000000000000000" and str(script.get("fileID", "0")) == "19102":
                type_name = "UnityEngine.UIElements.UIDocument"
            metadata = {"m_ObjectHideFlags", "m_GameObject", "m_Enabled", "m_EditorHideFlags", "m_Script", "m_Name", "m_EditorClassIdentifier"}
            scalar = {key: value for key, value in data.items() if key not in metadata}
        else:
            scalar = data
        return {"fileId": obj["fileId"], "classId": obj["classId"], "gameObjectId": str(data.get("m_GameObject", {}).get("fileID", "0")),
                "sourcePath": obj["sourcePath"], "guid": obj["guid"], "typeName": type_name,
                "skipMissingScript": obj.get("_missingScript", False),
                "dataJson": _json(data), "scalarDataJson": _json(_scalar_data(scalar)), "references": list(_references(data))}

    def validate_objects(self, objects):
        by_id = {o["fileId"]: o for o in objects}
        if len(by_id) != len(objects):
            self.error("duplicate-expanded-id", self.project / objects[0]["sourcePath"], "Prefab expansion produced duplicate IDs")
        for obj in objects:
            path, data, class_id = self.source(obj["sourcePath"]), obj["data"], obj["classId"]
            self.validate_native_settings(obj)
            if class_id not in SUPPORTED_CLASSES:
                self.error("native-class", path, f"Class {class_id} {obj['typeName']} has no raw runtime adapter", obj["fileId"])
            if class_id in (29, 157, 196):
                for reference in _references(data):
                    if reference["fileId"] != "0":
                        if self.is_empty_lighting_placeholder(obj, reference):
                            continue
                        self.error("baked-scene-data", path, f"{obj['typeName']} has baked reference {reference['property']}; baking is unsupported", obj["fileId"])
                self.info("scene-metadata", path, f"{obj['typeName']} contains authoring metadata only; no baked data is loaded", obj["fileId"])
            for reference in _references(data):
                file_id, guid = reference["fileId"], reference["guid"]
                if obj.get("_missingScript") and reference["property"] == "m_Script":
                    continue
                if file_id == "0":
                    continue
                if guid in BUILTIN_GUIDS:
                    if self.is_empty_lighting_placeholder(obj, reference):
                        continue
                    allowed = (guid == "0000000000000000e000000000000000" and file_id in {"10202", "10206", "10207", "10208", "10209", "10210"}) or (guid == "0000000000000000f000000000000000" and file_id == "10303")
                    allowed |= class_id == 104 and ((guid == "0000000000000000f000000000000000" and file_id == "10304") or (reference["property"] == "m_SpotCookie" and guid == "0000000000000000e000000000000000" and file_id == "10001"))
                    allowed |= class_id == 114 and reference["property"] == "m_Script" and guid == "0000000000000000e000000000000000" and file_id == "19102"
                    if not allowed:
                        self.error("builtin-reference", path, f"Unsupported builtin {guid}/{file_id} at {reference['property']}", obj["fileId"])
                    continue
                if guid:
                    self.asset(guid, path, file_id)
                elif file_id not in by_id:
                    self.error("dangling-reference", path, f"{reference['property']} points to missing object {file_id}", obj["fileId"])
            if class_id not in (1, 104, 29, 157, 196, 1660057539):
                game_object = data.get("m_GameObject", {})
                target = by_id.get(str(game_object.get("fileID", "0")))
                if target is None or target["classId"] != 1:
                    self.error("component-gameobject", path, "Component requires a valid scene GameObject", obj["fileId"])
            if class_id == 1:
                component_ids = [str(entry.get("component", {}).get("fileID", "0")) for entry in data.get("m_Component", [])]
                transforms = [by_id[i] for i in component_ids if i in by_id and by_id[i]["classId"] in TRANSFORM_CLASSES]
                if len(transforms) != 1:
                    self.error("gameobject-transform", path, "GameObject must have exactly one Transform", obj["fileId"])
                for component_id in component_ids:
                    component = by_id.get(component_id)
                    if component and str(component["data"].get("m_GameObject", {}).get("fileID", "0")) != obj["fileId"]:
                        self.error("component-owner", path, f"Component {component_id} belongs to a different GameObject", obj["fileId"])
            if class_id == 114 and not data.get("m_Script", {}).get("guid"):
                self.error("mono-script", path, "MonoBehaviour requires a raw m_Script GUID", obj["fileId"])
            if class_id in TRANSFORM_CLASSES:
                parent = str(data.get("m_Father", {}).get("fileID", "0"))
                if parent != "0" and parent in by_id and by_id[parent]["classId"] not in TRANSFORM_CLASSES:
                    self.error("transform-parent", path, "Transform parent must reference a Transform", obj["fileId"])
        for obj in objects:
            if obj["classId"] not in TRANSFORM_CLASSES:
                continue
            seen, current = set(), obj
            while current is not None:
                if current["fileId"] in seen:
                    self.error("transform-cycle", self.source(obj["sourcePath"]), "Transform hierarchy contains a cycle", obj["fileId"])
                    break
                seen.add(current["fileId"])
                current = by_id.get(str(current["data"].get("m_Father", {}).get("fileID", "0")))

    def is_empty_lighting_placeholder(self, obj, reference):
        if obj["classId"] != 157 or reference["property"] != "m_LightingDataAsset" or reference["guid"] != "0000000000000000f000000000000000" or reference["fileId"] != "20201":
            return False
        return any(metadata.get("scenePath") == obj["sourcePath"] and type(metadata.get("lightmapsCount")) is int and metadata["lightmapsCount"] == 0
                   and type(metadata.get("bakedRenderersCount")) is int and metadata["bakedRenderersCount"] == 0 and metadata.get("capturedInPlayMode") is False
                   for metadata in self.native_catalog.get("nativeSceneMetadata", []))


def _scene_path(project: Path, scene: str) -> Path:
    candidate = project / scene
    if candidate.is_file() and candidate.suffix.lower() == ".unity":
        result = candidate.resolve()
    else:
        matches = sorted((project / "Assets").rglob(scene if scene.endswith(".unity") else scene + ".unity"))
        if len(matches) != 1:
            raise RawSceneError(f"Scene {scene!r} resolves to {len(matches)} files; supply an exact Assets/...unity path")
        result = matches[0].resolve()
    if not _within(result, project):
        raise RawSceneError("Raw scene must be inside the project")
    return result


def _asset_ids(asset):
    if asset.get("kind") == "imported":
        return {item["fileId"] for item in asset.get("_nativeObjects", [])}
    if asset.get("kind") == "prefab":
        return {item["fileId"] for item in asset.get("objects", [])} | {item["fileId"] for item in asset.get("objectAliases", [])}
    return {asset["fileId"]}


def validate_scene(project: Path, scene: str, native_catalog=None, *, session_cache=None, source_inventory=None,
                   catalog_is_owned=False) -> dict:
    """Return a complete source/dependency report; never invokes Unity."""
    project = Path(project).resolve()
    path = _scene_path(project, scene)
    reader = _Reader(project, native_catalog, session_cache=session_cache, source_inventory=source_inventory,
                     catalog_is_owned=catalog_is_owned)
    try:
        objects, _ = reader.expand(path)
        reader.validate_objects(objects)
        for binding in reader.native_catalog.get("deferredBindings", []):
            owner_guid = str(binding.get("ownerGuid", ""))
            owner = reader.native_assets.get(owner_guid, [])
            if not any(entry["fileId"] == str(binding.get("ownerFileId", "")) for entry in owner):
                reader.error("native-binding-owner", path, f"Deferred binding owner {owner_guid}/{binding.get('ownerFileId')} is absent from the native catalog")
                continue
            reader.asset(owner_guid, path, str(binding.get("ownerFileId", "")))
            target_guid = str(binding.get("targetGuid", ""))
            target_id = str(binding.get("targetFileId", "0"))
            target_source = reader.resolve(target_guid, path, target_id)
            if target_source is not None and binding.get("targetPath") != reader.relative(target_source):
                reader.error("native-binding-path", path, f"Deferred binding target {target_guid} path differs from its authored metadata")
            reader.asset(target_guid, path, target_id)
            target = reader.assets.get(target_guid)
            if target is not None and target_id not in _asset_ids(target):
                reader.error("native-binding-fileid", path, f"Deferred binding target {target_guid}/{target_id} is absent from the authored prefab graph")
        objects = reader.order_scene_roots(objects, path)
    except (RawSceneError, OSError, ValueError) as exc:
        reader.error("native-import-required" if isinstance(exc, RawNativeClosureError) else "parse-error",
                     exc.source if isinstance(exc, RawNativeClosureError) else path, str(exc))
        objects = []
    for obj in objects:
        for reference in _references(obj["data"]):
            asset = reader.assets.get(reference["guid"])
            if asset is not None and reference["fileId"] != "0" and reference["fileId"] not in _asset_ids(asset):
                reader.error("asset-fileid", reader.source(obj["sourcePath"]), f"Reference {reference['property']} requests {reference['fileId']}, but raw asset {asset['path']} exposes {sorted(_asset_ids(asset))}", obj["fileId"])
    output = [reader.manifest_object(obj) for obj in objects]
    version_file = project / "ProjectSettings/ProjectVersion.txt"
    reader.depend(version_file)
    version = re.search(r"^m_EditorVersion:\s*(\S+)", reader.text(version_file), re.MULTILINE) if version_file.is_file() else None
    if not version:
        reader.error("unity-version", path, "ProjectSettings/ProjectVersion.txt must declare m_EditorVersion")
    if reader.native_catalog and version and reader.native_catalog.get("engineVersion", version.group(1)) != version.group(1):
        reader.error("native-catalog-version", path, f"Native catalog engine {reader.native_catalog.get('engineVersion')} differs from raw project {version.group(1)}")
    settings_files = sorted((project / "ProjectSettings").glob("*.asset"))
    pipeline = {"fileID": "0", "guid": ""}
    quality_name = ""
    preloaded_assets = []
    global_settings = []
    if reader.native_catalog:
        graphics = project / "ProjectSettings/GraphicsSettings.asset"
        quality = project / "ProjectSettings/QualitySettings.asset"
        player = project / "ProjectSettings/ProjectSettings.asset"
        def setting_reference(value, source, property_path):
            if not isinstance(value, dict) or "fileID" not in value:
                raise ValueError(f"{property_path} must contain a serialized Unity object reference")
            reference = {"guid": str(value.get("guid", "")), "fileId": str(value["fileID"]), "type": int(value.get("type", 0))}
            if reference["fileId"] == "0":
                return reference
            if not reference["guid"] or reference["guid"] in BUILTIN_GUIDS:
                reader.error("project-settings-reference", source, f"{property_path} requires an explicitly supported persistent asset: {reference}")
                return reference
            reader.asset(reference["guid"], source, reference["fileId"])
            target = reader.assets.get(reference["guid"])
            if target is not None and reference["fileId"] not in _asset_ids(target):
                reader.error("project-settings-fileid", source, f"{property_path} requests absent asset {reference['guid']}/{reference['fileId']}")
            return reference
        try:
            if graphics.is_file():
                settings = reader.documents(graphics)[0]["data"]
                pipeline = settings.get("m_CustomRenderPipeline", settings.get("m_RenderPipelineAsset", settings.get("defaultRenderPipeline", pipeline)))
                settings_map = settings.get("m_RenderPipelineGlobalSettingsMap", {})
                if not isinstance(settings_map, dict):
                    raise ValueError("m_RenderPipelineGlobalSettingsMap must be a type-to-asset mapping")
                for type_name, value in settings_map.items():
                    if not isinstance(value, dict):
                        raise ValueError(f"Global settings for {type_name} require a serialized object reference")
                    if str(value.get("fileID", "0")) == "0":
                        continue
                    reference = setting_reference(value, graphics, "m_RenderPipelineGlobalSettingsMap." + type_name)
                    global_settings.append({"pipelineTypeName": type_name, "guid": reference["guid"], "fileId": reference["fileId"]})
            if quality.is_file():
                settings = reader.documents(quality)[0]["data"]
                levels = settings.get("m_QualitySettings", [])
                default_levels = settings.get("m_PerPlatformDefaultQuality", {})
                index = int(default_levels.get("Standalone", settings.get("m_CurrentQuality", 0)))
                if not 0 <= index < len(levels):
                    raise ValueError(f"Standalone quality index {index} is outside available levels")
                quality_name = levels[index].get("name", "")
                candidate = levels[index].get("customRenderPipeline", {"fileID": "0"})
                if str(candidate.get("fileID", "0")) != "0":
                    pipeline = candidate
            if player.is_file():
                settings = reader.documents(player)[0]["data"]
                preloads = settings.get("preloadedAssets", [])
                if not isinstance(preloads, list):
                    raise ValueError("PlayerSettings.preloadedAssets must be a serialized asset-reference array")
                for index, value in enumerate(preloads):
                    preloaded_assets.append(setting_reference(value, player, f"preloadedAssets[{index}]"))
                if "preloadedAssets" in reader.native_catalog:
                    authored = [(item["guid"], item["fileId"]) for item in preloaded_assets if item["fileId"] != "0"]
                    observed = [(str(item.get("guid", "")), str(item.get("fileId", ""))) for item in reader.native_catalog["preloadedAssets"]]
                    if authored != observed:
                        reader.error("native-preload-consistency", player, "Native importer preload identities/order differ from authored ProjectSettings; refresh imported assets")
            if pipeline.get("guid") and str(pipeline.get("fileID", "0")) != "0":
                reader.asset(pipeline["guid"], graphics, str(pipeline["fileID"]))
                target = reader.assets.get(pipeline["guid"])
                if target is not None and str(pipeline["fileID"]) not in _asset_ids(target):
                    reader.error("render-pipeline-reference", graphics, f"Pipeline {pipeline} is absent from the imported catalog")
        except (RawSceneError, KeyError, IndexError, ValueError, TypeError) as exc:
            reader.error("render-pipeline-settings", graphics, str(exc))
    # Project preloads and pipeline settings can introduce assets that have no
    # references from the scene. Validate the complete discovered graph only
    # after those roots have been expanded.
    for asset in list(reader.assets.values()):
        for reference in asset.get("references", []):
            target = reader.assets.get(reference["guid"])
            if target is not None and reference["fileId"] != "0" and reference["fileId"] not in _asset_ids(target):
                reader.error("asset-fileid", reader.source(asset["path"]), f"Asset reference {reference['property']} requests invalid fileID {reference['fileId']}", asset["fileId"])
    if settings_files:
        settings_message = "Prepared host defaults are used for project settings fields outside the supported adapter: " if reader.native_catalog else "Prepared host defaults are used; raw project settings are not imported: "
        reader.info("project-settings-not-imported", path, settings_message + ", ".join(file.name for file in settings_files), severity="warning")
    dependencies = sorted(reader.dependencies.values(), key=lambda item: item["path"])
    content_hash = hashlib.sha256(_json(dependencies).encode()).hexdigest()
    assets = []
    for descriptor in reader.assets.values():
        assets.extend(descriptor.get("_nativeObjects", [descriptor]))
    return {"schemaVersion": 1, "unityVersion": version.group(1) if version else "", "sceneName": path.stem,
            "scenePath": reader.relative(path), "projectRoot": str(project), "assemblies": [],
            "objects": output, "assets": sorted(assets, key=lambda item: (item["guid"], item["fileId"])),
            "importedAssetPacks": reader.native_catalog.get("packs", []), "importedAssetCatalog": reader.native_catalog_path,
            "importedAssetBindings": reader.native_catalog.get("deferredBindings", []),
            "renderPipelineGuid": pipeline.get("guid", ""), "renderPipelineFileId": str(pipeline.get("fileID", "0")),
            "qualityName": quality_name,
            "preloadedAssets": preloaded_assets, "renderPipelineGlobalSettings": global_settings,
            "projectSettingDependencies": [item for item in dependencies if item["path"].startswith("ProjectSettings/")],
            "requiresScriptableObjectBarrier": reader.requires_scriptable_object_barrier,
            "emptyBuiltinLightingDataVerified": any(reader.is_empty_lighting_placeholder(obj, reference)
                                                     for obj in objects for reference in _references(obj["data"])),
            "sourceDependencies": dependencies, "contentHash": content_hash, "diagnostics": reader.diagnostics,
            "nativeImportRequirements": reader.native_import_requirements,
            "supported": not any(item["severity"] == "error" for item in reader.diagnostics),
            "requiresEditor": False, "requiresLibrary": False,
            "requiresCookedContent": bool(reader.native_catalog.get("packs")),
            "usesUnityImportedAssets": bool(reader.native_catalog.get("packs"))}


def prepare_scene(project: Path, scene: str, output: Path, native_catalog=None, *, session_cache=None, source_inventory=None,
                  catalog_is_owned=False) -> dict:
    """Validate and atomically publish a runtime manifest, or raise with report."""
    manifest = validate_scene(project, scene, native_catalog, session_cache=session_cache, source_inventory=source_inventory,
                              catalog_is_owned=catalog_is_owned)
    if not manifest["supported"]:
        errors = [d for d in manifest["diagnostics"] if d["severity"] == "error"]
        raise RawSceneError(f"Raw scene has {len(errors)} unsupported or invalid content entries: {errors[0]['message']}", manifest)
    output = Path(output)
    if output.is_dir() or not output.suffix:
        output = output / "manifest.json"
    output.parent.mkdir(parents=True, exist_ok=True)
    temporary = output.with_name(output.name + ".tmp")
    temporary.write_text(json.dumps(manifest, indent=2, ensure_ascii=False, allow_nan=False) + "\n")
    temporary.replace(output)
    manifest["manifest"] = str(output.resolve())
    return manifest
