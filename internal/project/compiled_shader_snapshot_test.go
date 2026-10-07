package goproject

import (
	"bytes"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

func newCompiledShaderSnapshotFixture(t *testing.T) (*Session, string, NativeReplacement) {
	t.Helper()
	s, output, r := newNativeShaderFixture(t)
	writeFixtureFile(t, filepath.Join(s.project, r.SourcePath), "Shader \"Test/Authored\" {\n#include \"Nested/A.hlsl\"\n}\n")
	writeFixtureFile(t, filepath.Join(s.project, "Assets/Nested/A.hlsl"), "#include \"Packages/test.pipeline/Runtime/B.hlsl\"\n#if OPTIONAL\n#include \"Absent.hlsl\"\n#endif\n")
	writeFixtureFile(t, filepath.Join(s.project, "Library/PackageCache/test.pipeline@abc/Runtime/B.hlsl"), "// #include \"Commented.hlsl\"\n#include \"Assets/Nested/A.hlsl\"\nfloat value = 1;\n")
	r.SourceSHA256 = nativeFixtureHash(t, filepath.Join(s.project, r.SourcePath))
	archive := filepath.Join(output, "compiled/origin.bundle")
	member := filepath.Join(output, "compiled/origin.sharedAssets")
	writeFixtureFile(t, archive, "verified compiled archive fixture")
	writeFixtureFile(t, member, "verified shader member fixture")
	requirements, missing, err := CollectShaderSourceRequirements(s.project, r.SourcePath)
	if err != nil {
		t.Fatal(err)
	}
	r.CompiledShaderSnapshot = &CompiledShaderSnapshot{SchemaVersion: 1, SourcePath: r.SourcePath, ShaderName: "Test/Authored", ArchiveFileID: "5597", BundlePath: archive, BundleSHA256: nativeFixtureHash(t, archive), MemberPath: member, MemberName: "origin.sharedAssets", MemberSHA256: nativeFixtureHash(t, member), PayloadSHA256: r.Content.SHA256, TypeTreeSHA256: r.Content.SHA256, SourceRequirements: requirements, MissingIncludes: missing}
	return s, output, r
}

func TestCompiledShaderSnapshotPublishesCompleteGuardAndOwnsDescriptor(t *testing.T) {
	s, output, r := newCompiledShaderSnapshotFixture(t)
	if len(r.CompiledShaderSnapshot.SourceRequirements) != 3 || !reflect.DeepEqual(r.CompiledShaderSnapshot.MissingIncludes, []string{"Assets/Nested/Absent.hlsl"}) {
		t.Fatal("nested, package, cyclic or conditional include closure was incorrect")
	}
	if err := s.ReplaceNativeContent(r); err != nil {
		t.Fatal(err)
	}
	r.CompiledShaderSnapshot.SourceRequirements[0].SHA256 = strings.Repeat("0", 64)
	r.CompiledShaderSnapshot.MissingIncludes[0] = "caller mutation"
	prepared, err := s.Prepare(output)
	if err != nil {
		t.Fatal(err)
	}
	manifest, err := ManifestJSON(prepared.Manifest)
	if err != nil {
		t.Fatal(err)
	}
	proofs := array(manifest["compiledShaderSnapshots"])
	if len(proofs) != 1 || object(proofs[0])["compiledSourceRevisionVerified"] != false || str(object(proofs[0])["guid"]) != r.GUID {
		t.Fatal("explicit snapshot provenance was lost")
	}
	paths := map[string]string{}
	for _, entry := range array(manifest["sourceDependencies"]) {
		d := object(entry)
		paths[str(d["path"])] = str(d["sourcePath"])
	}
	for _, requirement := range s.nativeOverlays[r.GUID].CompiledShaderSnapshot.SourceRequirements {
		if paths[requirement.Path] != requirement.SourcePath {
			t.Fatalf("missing current source guard %s", requirement.Path)
		}
	}
	for _, origin := range []string{r.CompiledShaderSnapshot.BundlePath, r.CompiledShaderSnapshot.MemberPath} {
		if paths[origin] != origin {
			t.Fatal("compiled origin was not pinned for prepared launch")
		}
	}
}

func TestCompiledShaderSnapshotRejectsTamperAndIncompleteClosure(t *testing.T) {
	for _, kind := range []string{"include mutation", "missing include appears", "omitted nested guard", "omitted missing guard", "unverified provenance claim", "archive mutation", "member mutation", "wrong source path", "duplicate guard", "foreign source"} {
		t.Run(kind, func(t *testing.T) {
			s, _, r := newCompiledShaderSnapshotFixture(t)
			before := clone(s.assets)
			switch kind {
			case "include mutation":
				writeFixtureFile(t, filepath.Join(s.project, "Assets/Nested/A.hlsl"), "new include body")
			case "missing include appears":
				writeFixtureFile(t, filepath.Join(s.project, "Assets/Nested/Absent.hlsl"), "new optional include")
			case "omitted nested guard":
				r.CompiledShaderSnapshot.SourceRequirements = r.CompiledShaderSnapshot.SourceRequirements[:2]
			case "omitted missing guard":
				r.CompiledShaderSnapshot.MissingIncludes = nil
			case "unverified provenance claim":
				r.CompiledShaderSnapshot.CompiledSourceRevisionVerified = true
			case "archive mutation":
				writeFixtureFile(t, r.CompiledShaderSnapshot.BundlePath, "new archive")
			case "member mutation":
				writeFixtureFile(t, r.CompiledShaderSnapshot.MemberPath, "new member")
			case "wrong source path":
				r.CompiledShaderSnapshot.SourcePath = "Assets/Other.shader"
			case "duplicate guard":
				r.CompiledShaderSnapshot.SourceRequirements = append(r.CompiledShaderSnapshot.SourceRequirements, r.CompiledShaderSnapshot.SourceRequirements[0])
			case "foreign source":
				r.CompiledShaderSnapshot.SourceRequirements[0].SourcePath = r.Content.Path
			}
			if err := s.ReplaceNativeContent(r); err == nil {
				t.Fatal("invalid compiled Shader snapshot accepted")
			}
			if len(s.nativeOverlays) != 0 || !reflect.DeepEqual(before, s.assets) {
				t.Fatal("rejected snapshot changed session content")
			}
		})
	}
}

func TestCompiledShaderSnapshotRechecksNestedIncludesBeforePrepareAndReplay(t *testing.T) {
	for _, kind := range []string{"include body", "absent include"} {
		t.Run(kind, func(t *testing.T) {
			s, output, r := newCompiledShaderSnapshotFixture(t)
			if err := s.ReplaceNativeContent(r); err != nil {
				t.Fatal(err)
			}
			first, err := s.Prepare(output)
			if err != nil {
				t.Fatal(err)
			}
			original, err := os.ReadFile(first.Manifest)
			if err != nil {
				t.Fatal(err)
			}
			if kind == "include body" {
				writeFixtureFile(t, filepath.Join(s.project, "Library/PackageCache/test.pipeline@abc/Runtime/B.hlsl"), "float value = 2;\n")
			} else {
				writeFixtureFile(t, filepath.Join(s.project, "Assets/Nested/Absent.hlsl"), "appeared\n")
			}
			if _, err := s.Prepare(output); err == nil {
				t.Fatal("accepted stale GPU shader after an include change")
			}
			if err := s.ReplaceNativeContent(r); err == nil {
				t.Fatal("cache replay accepted stale source/include guard")
			}
			current, err := os.ReadFile(first.Manifest)
			if err != nil || !bytes.Equal(original, current) {
				t.Fatal("failed preparation changed immutable revision")
			}
		})
	}
}

func TestShaderSnapshotRejectsDynamicIncludesAmbiguousPackagesAndEscapes(t *testing.T) {
	for _, kind := range []string{"macro", "multiple package versions", "escape"} {
		t.Run(kind, func(t *testing.T) {
			project := t.TempDir()
			body := "#include PLATFORM_INCLUDE\n"
			if kind == "multiple package versions" {
				body = "#include \"Packages/test.pipeline/B.hlsl\"\n"
				for _, version := range []string{"a", "b"} {
					writeFixtureFile(t, filepath.Join(project, "Library/PackageCache/test.pipeline@"+version+"/B.hlsl"), "body")
				}
			}
			if kind == "escape" {
				body = "#include \"../../Outside.hlsl\"\n"
			}
			writeFixtureFile(t, filepath.Join(project, "Assets/Test.shader"), body)
			if _, _, err := CollectShaderSourceRequirements(project, "Assets/Test.shader"); err == nil {
				t.Fatal("unsupported include resolution was accepted")
			}
		})
	}
}
