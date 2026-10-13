// Portable PDB of a patch assembly, so that a stack trace through patched code shows file and line.
//
// Bodies are copied without moving an instruction, so the sequence points of the new compilation
// apply unchanged; only an added instance method has a receiver check in front (`Lead` bytes) that
// shifts them. What must be rewritten is every row number inside them: documents and the local
// signature are rows of another image.
using System.Reflection.Metadata;
using System.Reflection.Metadata.Ecma335;
using System.Security.Cryptography;

internal static class PatchPdb
{
    public static BlobContentId ContentId(IEnumerable<Blob> content)
    {
        using var hash = IncrementalHash.CreateHash(HashAlgorithmName.SHA256);
        foreach (var blob in content) hash.AppendData(blob.GetBytes());
        return BlobContentId.FromHash(hash.GetHashAndReset());
    }

    /// `methods` maps rows of the patch to the methods of the new compilation they were copied from.
    /// Null when the new compilation has no readable PDB or none of the methods has sequence points.
    public static (byte[] Image, BlobContentId Id, ushort Version)? Build(MetadataReader? source, MetadataBuilder typeSystem,
        List<(int Row, MethodDefinitionHandle Source, int Locals, int Lead)> methods)
    {
        if (source == null) return null;
        try
        {
            var pdb = new MetadataBuilder();
            var documents = new Dictionary<DocumentHandle, int>();
            int Document(DocumentHandle h)
            {
                if (documents.TryGetValue(h, out int row)) return row;
                var d = source.GetDocument(h);
                var added = pdb.AddDocument(
                    d.Name.IsNil ? default : pdb.GetOrAddDocumentName(source.GetString(d.Name)),
                    d.HashAlgorithm.IsNil ? default : pdb.GetOrAddGuid(source.GetGuid(d.HashAlgorithm)),
                    d.Hash.IsNil ? default : pdb.GetOrAddBlob(source.GetBlobBytes(d.Hash)),
                    d.Language.IsNil ? default : pdb.GetOrAddGuid(source.GetGuid(d.Language)));
                return documents[h] = MetadataTokens.GetRowNumber(added);
            }

            var byRow = methods.ToDictionary(m => m.Row);
            int count = typeSystem.GetRowCount(TableIndex.MethodDef), described = 0;
            // One row per method of the patch, in method order.
            for (int row = 1; row <= count; row++)
            {
                var points = new List<SequencePoint>();
                if (byRow.TryGetValue(row, out var method) && MetadataTokens.GetRowNumber(method.Source) <= source.MethodDebugInformation.Count)
                {
                    var info = source.GetMethodDebugInformation(method.Source);
                    if (!info.SequencePointsBlob.IsNil)
                    {
                        int last = -1;
                        foreach (var point in info.GetSequencePoints())
                        {
                            // A second record at the same offset would read back as a document switch.
                            if (point.Offset <= last || point.Document.IsNil) continue;
                            points.Add(point);
                            last = point.Offset;
                        }
                    }
                }
                if (points.Count == 0) { pdb.AddMethodDebugInformation(default, default); continue; }
                bool oneDocument = points.TrueForAll(p => p.Document == points[0].Document);
                var blob = Encode(points, method.Locals, method.Lead, oneDocument, Document);
                pdb.AddMethodDebugInformation(oneDocument ? MetadataTokens.DocumentHandle(Document(points[0].Document)) : default, pdb.GetOrAddBlob(blob));
                described++;
            }
            if (described == 0) return null;
            var builder = new PortablePdbBuilder(pdb, typeSystem.GetRowCounts(), default, ContentId);
            var image = new BlobBuilder();
            var id = builder.Serialize(image);
            return (image.ToArray(), id, builder.FormatVersion);
        }
        catch (BadImageFormatException)
        {
            return null; // symbols are a convenience: a damaged PDB must not stop a patch
        }
    }

    /// The sequence points blob of the Portable PDB format.
    /// A point at the start of the method stays there and so covers the lead too.
    static BlobBuilder Encode(List<SequencePoint> points, int localSignatureRow, int lead, bool oneDocument, Func<DocumentHandle, int> document)
    {
        var w = new BlobBuilder();
        w.WriteCompressedInteger(localSignatureRow);
        var current = points[0].Document;
        if (!oneDocument) w.WriteCompressedInteger(document(current));
        int previousOffset = 0, previousLine = -1, previousColumn = -1;
        for (int i = 0; i < points.Count; i++)
        {
            var p = points[i];
            if (p.Document != current)
            {
                w.WriteCompressedInteger(0);
                w.WriteCompressedInteger(document(p.Document));
                current = p.Document;
            }
            int offset = p.Offset == 0 ? 0 : p.Offset + lead;
            w.WriteCompressedInteger(i == 0 ? offset : offset - previousOffset);
            previousOffset = offset;
            if (p.IsHidden)
            {
                w.WriteCompressedInteger(0);
                w.WriteCompressedInteger(0);
                continue;
            }
            int lines = p.EndLine - p.StartLine, columns = p.EndColumn - p.StartColumn;
            w.WriteCompressedInteger(lines);
            if (lines == 0) w.WriteCompressedInteger(columns);
            else w.WriteCompressedSignedInteger(columns);
            if (previousLine < 0)
            {
                w.WriteCompressedInteger(p.StartLine);
                w.WriteCompressedInteger(p.StartColumn);
            }
            else
            {
                w.WriteCompressedSignedInteger(p.StartLine - previousLine);
                w.WriteCompressedSignedInteger(p.StartColumn - previousColumn);
            }
            previousLine = p.StartLine;
            previousColumn = p.StartColumn;
        }
        return w;
    }
}
