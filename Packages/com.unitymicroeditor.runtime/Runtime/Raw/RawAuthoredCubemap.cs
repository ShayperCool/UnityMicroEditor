using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using UnityEngine;
using Object = UnityEngine.Object;

namespace UnityMicroEditor.Raw
{
    public static partial class RawSceneLoader
    {
        private static class RawAuthoredCubemap
        {
            private static int Number(Dictionary<string, object> data, string key) => Convert.ToInt32(data[key], CultureInfo.InvariantCulture);
            internal static void Validate(RawSceneAsset asset)
            {
                var data = (Dictionary<string, object>)RawJson.Parse(asset.dataJson);
                int width = Number(data, "m_Width");
                if (asset.classId != 89 || Number(data, "serializedVersion") != 4 || width <= 0 || width > 2048
                    || Number(data, "m_Height") != width || Number(data, "m_TextureFormat") != 17 || Number(data, "m_MipCount") != 1
                    || Number(data, "m_ColorSpace") != 0
                    || Number(data, "m_ImageCount") != 6 || Number(data, "m_TextureDimension") != 4 || Number(data, "m_IsReadable") != 1
                    || ((string)data["_typelessdata"]).Length != checked(6 * width * width * 8 * 2))
                    throw new InvalidDataException("Unsupported authored Cubemap: " + asset.path);
            }
            internal static Cubemap Create(RawSceneAsset asset)
            {
                var data = (Dictionary<string, object>)RawJson.Parse(asset.dataJson);
                int size = Number(data, "m_Width");
                string hex = (string)data["_typelessdata"];
                byte[] bytes = new byte[hex.Length / 2];
                for (int i = 0; i < bytes.Length; i++) bytes[i] = byte.Parse(hex.Substring(i * 2, 2), NumberStyles.HexNumber, CultureInfo.InvariantCulture);
                var cube = new Cubemap(size, TextureFormat.RGBAHalf, false) { name = asset.name };
                try
                {
                    if (cube.format != TextureFormat.RGBAHalf || cube.mipmapCount != 1 || cube.isDataSRGB)
                        throw new InvalidDataException("Cubemap runtime format/color space differs from authored HDR data.");
                    var settings = (Dictionary<string, object>)data["m_TextureSettings"];
                    cube.filterMode = (FilterMode)Number(settings, "m_FilterMode"); cube.anisoLevel = Number(settings, "m_Aniso");
                    cube.wrapModeU = (TextureWrapMode)Number(settings, "m_WrapU"); cube.wrapModeV = (TextureWrapMode)Number(settings, "m_WrapV");
                    cube.wrapModeW = (TextureWrapMode)Number(settings, "m_WrapW");
                    cube.mipMapBias = Convert.ToSingle(settings["m_MipBias"], CultureInfo.InvariantCulture);
                    int faceBytes = checked(size * size * 8);
                    for (int face = 0; face < 6; face++) cube.SetPixelData(bytes, 0, (CubemapFace)face, face * faceBytes);
                    cube.Apply(false, false);
                    for (int face = 0; face < 6; face++)
                    {
                        var actual = cube.GetPixelData<byte>(0, (CubemapFace)face);
                        if (actual.Length != faceBytes) throw new InvalidDataException("Cubemap face byte count changed.");
                        for (int i = 0; i < faceBytes; i++)
                            if (actual[i] != bytes[face * faceBytes + i]) throw new InvalidDataException("Cubemap pixel readback differs from authored bytes.");
                    }
                    return cube;
                }
                catch { Object.Destroy(cube); throw; }
            }
        }
    }
}
