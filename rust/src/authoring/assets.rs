use super::{
    ScriptRequest,
    hierarchy::{field, id},
    script::new_guid,
    store::*,
    yaml_edit::TextAsset,
};
use serde_json::json;
use std::{collections::BTreeMap, fs, io, path::Path};
pub type AssetCreateRequest = ScriptRequest;
impl Store {
    pub fn create_asset(&self, request: &AssetCreateRequest) -> Result<MutationResult> {
        self.asset_path(&request.path, &[".mat", ".asset", ".prefab"])?;
        if request.expected_revision != "absent"
            || request.content.is_empty()
            || request.content.len() > MAX_SOURCE
            || request.content.contains('\0')
        {
            return Err(failure(
                "invalid_request",
                "asset creation requires expectedRevision absent and nonempty Unity text at most 32 MiB",
                None,
            ));
        }
        if !request.guid.is_empty() {
            new_guid(&request.guid).map_err(|_| {
                failure(
                    "invalid_identity",
                    "new asset GUID must be nonzero lowercase hexadecimal",
                    None,
                )
            })?;
        }
        self.mutate(&request.path,&request.expected_revision,&request.operation_id,request,||{
            let absolute=self.asset_path(&request.path,&[".mat",".asset",".prefab"])?;
            for path in [absolute.clone(),self.safe_path(&format!("{}.meta",request.path),false)?]{
                match fs::symlink_metadata(path){Ok(_)=>return Err(failure("revision_conflict","new asset or its metadata already exists",None)),Err(e) if e.kind()==io::ErrorKind::NotFound=>(),Err(e)=>return Err(e.into())}
            }
            if !absolute.parent().unwrap().is_dir(){return Err(failure("invalid_path","asset parent directory must already exist",None));}
            let asset=TextAsset::parse(&request.content)?;
            let guid=new_guid(&request.guid)?;self.check_guid_available(&guid)?;
            asset.validate(&guid)?;
            let extension=Path::new(&request.path).extension().and_then(|v|v.to_str()).unwrap_or_default();
            if extension=="prefab"{asset.validate_prefab_root()?;}
            let mut main_id=String::new();
            for document in &asset.docs{
                let document=&document.document;
                match extension{
                    "mat"=>{
                        if document.class_id==21&&main_id.is_empty(){main_id=document.file_id.clone();}
                        else if document.class_id!=114||field(&document.data,"m_EditorClassIdentifier").as_str()!=Some("Unity.RenderPipelines.Universal.Editor::UnityEditor.Rendering.Universal.AssetVersion"){return Err(failure("unsupported_asset","material requires one class21 object and only supported version subassets",None));}
                    }
                    "asset"=>{
                        if document.class_id!=114||id(field(&document.data,"m_GameObject"))!="0"||id(field(&document.data,"m_Script"))=="0"{return Err(failure("unsupported_asset","ScriptableObject asset requires class114 objects with explicit MonoScript references and no GameObject owner",None));}
                        if main_id.is_empty(){main_id=document.file_id.clone();}
                    }
                    "prefab" if main_id.is_empty()&&((document.class_id==1&&!document.stripped)||document.class_id==1001)=>{main_id=document.file_id.clone();}
                    _=>(),
                }
            }
            if main_id.is_empty(){return Err(failure("unsupported_asset","asset has no supported main object",None));}
            let importer=if extension=="prefab"{"PrefabImporter:\n  externalObjects: {}\n  userData: \n  assetBundleName: \n  assetBundleVariant: \n".into()}else{format!("NativeFormatImporter:\n  externalObjects: {{}}\n  mainObjectFileID: {main_id}\n  userData: \n  assetBundleName: \n  assetBundleVariant: \n")};
            Ok(MutationPlan {source:Source {path:request.path.clone(),revision:"absent".into(),..Source::default()},content:request.content.as_bytes().to_vec(),metadata:format!("fileFormatVersion: 2\nguid: {guid}\n{importer}").into_bytes(),created:BTreeMap::new(),changes:vec![Change {action:"create_asset".into(),after:Some(json!(request.path)),..Change::default()}]})
        })
    }
}
