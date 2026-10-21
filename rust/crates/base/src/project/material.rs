//! The version stamps the render pipeline packages keep beside a material,
//! which a source tool leaves out of a material's objects.

/// The version stamps the render pipeline packages keep next to a material:
/// a hidden `AssetVersion` object of an Editor assembly, by the GUID of its
/// script and by the class the Editor writes beside it. A Player has no such
/// class and a build writes none of these objects, so they are left out.
const VERSION_STAMPS: [(&str, &str); 3] = [
    (
        "d0353a89b1f911e48b9e16bdc9f2e058",
        "Unity.RenderPipelines.Universal.Editor::UnityEditor.Rendering.Universal.AssetVersion",
    ),
    (
        "639247ca83abc874e893eb93af2b5e44",
        "Unity.ShaderGraph.Editor::UnityEditor.Rendering.BuiltIn.AssetVersion",
    ),
    (
        "da692e001514ec24dbc4cca1949ff7e8",
        "Unity.RenderPipelines.HighDefinition.Editor::UnityEditor.Rendering.HighDefinition.AssetVersion",
    ),
];

/// Whether a `MonoBehaviour` document of a material source is one of the
/// version stamps. An Editor before the class identifier existed left it
/// empty, and then the script alone names the class; where both are written
/// they have to agree. An object some GameObject owns is not a stamp.
pub fn editor_version_stamp(
    identifier: &str,
    script_file_id: &str,
    script_guid: &str,
    owner_file_id: &str,
) -> bool {
    if owner_file_id != "0" {
        return false;
    }
    let by_script = VERSION_STAMPS
        .iter()
        .find(|(guid, _)| *guid == script_guid && script_file_id == "11500000");
    match by_script {
        Some((_, class)) => identifier.is_empty() || identifier == *class,
        // A source without the script reference is not what the Editor
        // writes; the class alone is accepted as it was before.
        None => {
            script_file_id == "0"
                && script_guid.is_empty()
                && VERSION_STAMPS.iter().any(|(_, class)| *class == identifier)
        }
    }
}
