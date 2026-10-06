using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;

// Each mode should be launched in a separate process with a caller-enforced timeout.
// These probes read project/build inputs; all writes go into an explicitly supplied output directory.
try
{
    object result = args[0] switch
    {
        "editor-api" => ProbeEditorApi(),
        "native-editor" => ProbeNativeEditor(args[1]),
        "ufs" => ProbeFileSystem(args[1], args[2], args[3], args[4]),
        "ufs-raw-format" => ProbeRawFormat(args[1], args[2]),
        _ => throw new ArgumentException("Modes: editor-api | native-editor <Unity> | ufs <UnityFileSystemApi.so> <raw.unity> <bundle> <output-dir>")
    };
    Console.WriteLine(JsonSerializer.Serialize(result, new JsonSerializerOptions { WriteIndented = true }));
    return 0;
}
catch (Exception error)
{
    Console.WriteLine(JsonSerializer.Serialize(new { mode = args.FirstOrDefault(), error = error.GetType().FullName, message = error.Message }));
    return 1;
}

[MethodImpl(MethodImplOptions.NoInlining)]
static object ProbeEditorApi()
{
    // Public binding: compiling/loading the DLL does not initialize the Editor native host.
    return new { guid = UnityEditor.AssetDatabase.AssetPathToGUID("Assets/Scenes/SampleScene.unity") };
}

static object ProbeNativeEditor(string path)
{
    var handle = NativeLibrary.Load(Path.GetFullPath(path));
    NativeLibrary.Free(handle);
    return new { loaded = true };
}

static object ProbeRawFormat(string library, string archivePath)
{
    NativeLibrary.SetDllImportResolver(typeof(Ufs).Assembly, (name, _, _) =>
        name == "UnityFileSystemApi" ? NativeLibrary.Load(Path.GetFullPath(library)) : IntPtr.Zero);
    Ufs.Check(Ufs.Init(), "init");
    Ufs.Check(Ufs.MountArchive(Path.GetFullPath(archivePath), "raw:/", out var archive), "mount");
    // Malformed/unsupported input can crash native code. Run ONLY as an isolated child.
    int result = Ufs.OpenSerializedFile("raw:/SampleScene.unity", out var serialized);
    Console.Error.WriteLine($"OpenSerializedFile returned {result}");
    // Installed 6000.6.3f1 returns 6 for this YAML, then crashes during cleanup.
    // On a failed parse, let this isolated process exit and reclaim native state.
    if (result != 0) return new { returnCode = result, cleanupSkippedOnFailedParse = true };
    Ufs.Check(Ufs.CloseSerializedFile(serialized), "close");
    Ufs.Check(Ufs.UnmountArchive(archive), "unmount");
    Ufs.Check(Ufs.Cleanup(), "cleanup");
    return new { returnCode = result };
}

static object ProbeFileSystem(string library, string rawScene, string cookedBundle, string outputDirectory)
{
    NativeLibrary.SetDllImportResolver(typeof(Ufs).Assembly, (name, _, _) =>
        name == "UnityFileSystemApi" ? NativeLibrary.Load(Path.GetFullPath(library)) : IntPtr.Zero);
    Ufs.Check(Ufs.Init(), "init");
    try
    {
        Directory.CreateDirectory(outputDirectory);
        // A byte container, deliberately NOT marked as a binary SerializedFile.
        var archivePath = Path.GetFullPath(Path.Combine(outputDirectory, "raw-scene.unityfs"));
        var source = Marshal.StringToCoTaskMemUTF8(Path.GetFullPath(rawScene));
        var alias = Marshal.StringToCoTaskMemUTF8("SampleScene.unity");
        int crc;
        try
        {
            Ufs.Check(Ufs.CreateArchive([source], [alias], [false], 1, archivePath, 2, out crc), "create archive");
        }
        finally
        {
            Marshal.FreeCoTaskMem(source);
            Marshal.FreeCoTaskMem(alias);
        }

        Ufs.Check(Ufs.MountArchive(archivePath, "raw:/", out var rawArchive), "mount raw archive");
        object rawResult;
        try
        {
            Ufs.Check(Ufs.OpenFile("raw:/SampleScene.unity", out var file), "open raw member");
            byte[] data;
            try
            {
                Ufs.Check(Ufs.GetFileSize(file, out var size), "raw size");
                data = new byte[checked((int)size)];
                Ufs.Check(Ufs.ReadFile(file, size, data, out var actual), "read raw member");
                if (actual != size || !data.AsSpan().SequenceEqual(File.ReadAllBytes(rawScene)))
                    throw new InvalidDataException("Raw archive round trip changed bytes.");
            }
            finally { Ufs.Check(Ufs.CloseFile(file), "close raw member"); }

            rawResult = new { archiveCreated = true, byteIdentical = true, bytes = data.Length,
                sha256 = Convert.ToHexString(SHA256.HashData(data)).ToLowerInvariant(),
                crc = unchecked((uint)crc), importedOrSerialized = false };
        }
        finally { Ufs.Check(Ufs.UnmountArchive(rawArchive), "unmount raw archive"); }

        Ufs.Check(Ufs.MountArchive(Path.GetFullPath(cookedBundle), "cooked:/", out var archive), "mount cooked bundle");
        var nodes = new List<object>();
        try
        {
            Ufs.Check(Ufs.GetArchiveNodeCount(archive, out var count), "node count");
            for (int index = 0; index < count; index++)
            {
                var name = new StringBuilder(4096);
                Ufs.Check(Ufs.GetArchiveNode(archive, index, name, name.Capacity, out var size, out var flags), "node");
                int? objects = null;
                if ((flags & 4) != 0)
                {
                    Ufs.Check(Ufs.OpenSerializedFile("cooked:/" + name, out var serialized), "open cooked serialized");
                    try
                    {
                        Ufs.Check(Ufs.GetObjectCount(serialized, out var objectCount), "object count");
                        objects = objectCount;
                    }
                    finally { Ufs.Check(Ufs.CloseSerializedFile(serialized), "close cooked serialized"); }
                }
                nodes.Add(new { name = name.ToString(), size, flags, objects });
            }
        }
        finally { Ufs.Check(Ufs.UnmountArchive(archive), "unmount cooked bundle"); }
        if (!nodes.Any()) throw new InvalidDataException("Cooked bundle contained no nodes.");
        return new { editorStartedByProbe = false, raw = rawResult, cookedBundleNodes = nodes };
    }
    finally { Ufs.Check(Ufs.Cleanup(), "cleanup"); }
}

static class Ufs
{
    // C ABI signatures match UnityDataTools' public DllWrapper.cs, pinned in NO_EDITOR_CONTENT.md.
    const string Library = "UnityFileSystemApi";
    [DllImport(Library, EntryPoint = "UFS_Init", CallingConvention = CallingConvention.Cdecl)]
    internal static extern int Init();
    [DllImport(Library, EntryPoint = "UFS_Cleanup", CallingConvention = CallingConvention.Cdecl)]
    internal static extern int Cleanup();
    [DllImport(Library, EntryPoint = "UFS_CreateArchive", CallingConvention = CallingConvention.Cdecl)]
    internal static extern int CreateArchive(IntPtr[] sourceFiles, IntPtr[] aliases, bool[] serialized, int count,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string path, int compression, out int crc);
    [DllImport(Library, EntryPoint = "UFS_MountArchive", CallingConvention = CallingConvention.Cdecl)]
    internal static extern int MountArchive([MarshalAs(UnmanagedType.LPUTF8Str)] string path,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string mountPoint, out IntPtr handle);
    [DllImport(Library, EntryPoint = "UFS_UnmountArchive", CallingConvention = CallingConvention.Cdecl)]
    internal static extern int UnmountArchive(IntPtr handle);
    [DllImport(Library, EntryPoint = "UFS_GetArchiveNodeCount", CallingConvention = CallingConvention.Cdecl)]
    internal static extern int GetArchiveNodeCount(IntPtr handle, out int count);
    [DllImport(Library, EntryPoint = "UFS_GetArchiveNode", CallingConvention = CallingConvention.Cdecl)]
    internal static extern int GetArchiveNode(IntPtr handle, int index, StringBuilder path, int capacity, out long size, out int flags);
    [DllImport(Library, EntryPoint = "UFS_OpenFile", CallingConvention = CallingConvention.Cdecl)]
    internal static extern int OpenFile([MarshalAs(UnmanagedType.LPUTF8Str)] string path, out IntPtr handle);
    [DllImport(Library, EntryPoint = "UFS_GetFileSize", CallingConvention = CallingConvention.Cdecl)]
    internal static extern int GetFileSize(IntPtr handle, out long size);
    [DllImport(Library, EntryPoint = "UFS_ReadFile", CallingConvention = CallingConvention.Cdecl)]
    internal static extern int ReadFile(IntPtr handle, long size, byte[] buffer, out long actualSize);
    [DllImport(Library, EntryPoint = "UFS_CloseFile", CallingConvention = CallingConvention.Cdecl)]
    internal static extern int CloseFile(IntPtr handle);
    [DllImport(Library, EntryPoint = "UFS_OpenSerializedFile", CallingConvention = CallingConvention.Cdecl)]
    internal static extern int OpenSerializedFile([MarshalAs(UnmanagedType.LPUTF8Str)] string path, out IntPtr handle);
    [DllImport(Library, EntryPoint = "UFS_CloseSerializedFile", CallingConvention = CallingConvention.Cdecl)]
    internal static extern int CloseSerializedFile(IntPtr handle);
    [DllImport(Library, EntryPoint = "UFS_GetObjectCount", CallingConvention = CallingConvention.Cdecl)]
    internal static extern int GetObjectCount(IntPtr handle, out int count);
    internal static void Check(int result, string operation)
    {
        if (result != 0) throw new IOException($"UFS {operation}: return code {result}");
    }
}
