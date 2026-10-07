package goproject

import (
	"encoding/hex"
	"fmt"
)

func (s *Session) refreshAuthoredCubemap(asset map[string]any) error {
	path := str(asset["path"])
	physical, err := s.source(path)
	if err != nil {
		return err
	}
	if err := s.depend(path); err != nil {
		return err
	}
	documents, err := s.parse(physical)
	if err != nil {
		return err
	}
	if len(documents) != 1 || documents[0].ClassID != 89 {
		return fmt.Errorf("raw cubemap %s needs one class89 document", path)
	}
	data := documents[0].Data
	width := int(number(data["m_Width"]))
	if number(data["serializedVersion"]) != 4 || width <= 0 || width > 2048 || number(data["m_Height"]) != int64(width) || number(data["m_TextureFormat"]) != 17 || number(data["m_ColorSpace"]) != 0 || number(data["m_MipCount"]) != 1 || number(data["m_MipsStripped"]) != 0 || number(data["m_ImageCount"]) != 6 || number(data["m_TextureDimension"]) != 4 || number(data["m_IsReadable"]) != 1 {
		return fmt.Errorf("raw cubemap %s requires readable RGBAHalf six-face single-mip source", path)
	}
	stream := object(data["m_StreamData"])
	if number(stream["size"]) != 0 || number(stream["offset"]) != 0 || str(stream["path"]) != "" || str(data["m_PlatformBlob"]) != "" || number(data["m_StreamingMipmaps"]) != 0 || number(data["m_VTOnly"]) != 0 {
		return fmt.Errorf("raw cubemap %s contains unsupported streamed/platform data", path)
	}
	pixels, err := hex.DecodeString(str(data["_typelessdata"]))
	if err != nil || len(pixels) != 6*width*width*8 || number(data["image data"]) != int64(len(pixels)) || number(data["m_CompleteImageSize"]) != int64(width*width*8) {
		return fmt.Errorf("raw cubemap %s has inconsistent authored pixel bytes", path)
	}
	asset["classId"], asset["typeName"], asset["fileId"], asset["name"] = 89, "UnityEngine.Cubemap", documents[0].FileID, str(data["m_Name"])
	asset["dataJson"], err = compactJSON(data)
	asset["references"] = referenceList(data)
	return err
}
