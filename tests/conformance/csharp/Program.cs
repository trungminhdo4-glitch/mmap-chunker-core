using System.Runtime.InteropServices;
using System.Reflection;

internal static class Native
{
    private const string ImportName = "mmap_chunker_core";
    private static string? libraryPath;
    private static bool resolverInstalled;

    [StructLayout(LayoutKind.Sequential)]
    internal struct CChunkView
    {
        internal IntPtr data;
        internal UIntPtr len;
    }

    internal static void Configure(string path)
    {
        libraryPath = Path.GetFullPath(path);
        if (!File.Exists(libraryPath))
            throw new InvalidOperationException($"native library does not exist: {libraryPath}");
        if (!resolverInstalled)
        {
            NativeLibrary.SetDllImportResolver(typeof(Native).Assembly, Resolve);
            resolverInstalled = true;
        }
    }

    private static IntPtr Resolve(string name, Assembly assembly, DllImportSearchPath? searchPath)
    {
        if (name == ImportName && libraryPath is not null)
            return NativeLibrary.Load(libraryPath);
        return IntPtr.Zero;
    }

    [DllImport(ImportName, CallingConvention = CallingConvention.Cdecl)]
    internal static extern uint mmap_engine_abi_version();

    [DllImport(ImportName, CallingConvention = CallingConvention.Cdecl)]
    internal static extern uint mmap_engine_capabilities();

    [DllImport(ImportName, CallingConvention = CallingConvention.Cdecl)]
    internal static extern IntPtr mmap_engine_last_error();

    [DllImport(ImportName, CallingConvention = CallingConvention.Cdecl, EntryPoint = "mmap_engine_open")]
    private static extern IntPtr OpenUtf8([MarshalAs(UnmanagedType.LPUTF8Str)] string path);

    [DllImport(ImportName, CallingConvention = CallingConvention.Cdecl)]
    internal static extern UIntPtr mmap_engine_partition_records(IntPtr handle, UIntPtr partitions, byte delimiter);

    [DllImport(ImportName, CallingConvention = CallingConvention.Cdecl)]
    internal static extern int mmap_engine_get_chunk(IntPtr handle, UIntPtr index, out CChunkView chunk);

    [DllImport(ImportName, CallingConvention = CallingConvention.Cdecl)]
    internal static extern void mmap_engine_free(IntPtr handle);

    internal static IntPtr Open(string path) => OpenUtf8(path);
}

internal static class Program
{
    private const ulong FnvOffset = 14695981039346656037;
    private const ulong FnvPrime = 1099511628211;

    private static void Fail(string message) => throw new InvalidOperationException(message);

    private static string LastError()
    {
        IntPtr pointer = Native.mmap_engine_last_error();
        return pointer == IntPtr.Zero ? string.Empty : Marshal.PtrToStringUTF8(pointer) ?? string.Empty;
    }

    private static (List<byte[]> Chunks, ulong Digest) Capture(IntPtr handle, byte[] source)
    {
        UIntPtr count = Native.mmap_engine_partition_records(handle, (UIntPtr)4, (byte)'\n');
        if (count == UIntPtr.Zero)
            Fail("unexpected partition count");
        ulong countValue = count.ToUInt64();
        var chunks = new List<byte[]>((int)countValue);
        ulong digest = FnvOffset;
        nuint offset = 0;
        for (nuint index = 0; index < (nuint)countValue; index++)
        {
            if (Native.mmap_engine_get_chunk(handle, (UIntPtr)index, out Native.CChunkView view) != 0)
                Fail("could not retrieve partition");
            ulong lengthValue = view.len.ToUInt64();
            if (view.data == IntPtr.Zero || lengthValue == 0 || lengthValue > (ulong)source.Length - (ulong)offset)
                Fail("invalid partition view");
            int length = checked((int)lengthValue);
            byte[] chunk = new byte[length];
            Marshal.Copy(view.data, chunk, 0, length);
            if (!source.AsSpan(checked((int)offset), length).SequenceEqual(chunk))
                Fail("partition bytes differ from fixture");
            if (index + 1 < (nuint)countValue && chunk[^1] != (byte)'\n')
                Fail("non-final partition splits a record");
            chunks.Add(chunk);
            foreach (byte value in chunk)
            {
                digest ^= value;
                digest *= FnvPrime;
            }
            offset += (nuint)length;
        }
        if (offset != (nuint)source.Length)
            Fail("partition plan does not reconstruct the fixture");
        return (chunks, digest);
    }

    private static void Main(string[] args)
    {
        string library = Required(args, "--library");
        string fixture = Required(args, "--fixture");
        string expected = Required(args, "--expected");
        string output = Required(args, "--output");
        Native.Configure(library);

        if (Marshal.SizeOf<Native.CChunkView>() != 16 ||
            Marshal.OffsetOf<Native.CChunkView>("data").ToInt32() != 0 ||
            Marshal.OffsetOf<Native.CChunkView>("len").ToInt32() != 8)
            Fail("CChunkView layout mismatch");
        if (Native.mmap_engine_abi_version() != 0x00010003 || Native.mmap_engine_capabilities() != 63)
            Fail("ABI discovery mismatch");

        byte[] source = File.ReadAllBytes(fixture);
        IntPtr handle = Native.Open(fixture);
        if (handle == IntPtr.Zero)
            Fail("mmap_engine_open failed for UTF-8 path");
        (List<byte[]> first, ulong digest) = Capture(handle, source);
        (List<byte[]> second, ulong repeatDigest) = Capture(handle, source);
        if (digest != repeatDigest || first.Count != second.Count ||
            first.Zip(second).Any(pair => !pair.First.SequenceEqual(pair.Second)))
            Fail("partition plan is not deterministic");
        if (Native.mmap_engine_partition_records(handle, UIntPtr.Zero, (byte)'\n') != UIntPtr.Zero)
            Fail("N=0 unexpectedly succeeded");
        string n0Error = LastError();
        if (n0Error != "requested_partitions must be > 0")
            Fail($"N=0 error contract mismatch: {n0Error}");
        Native.mmap_engine_free(handle);

        int recordCount = source.Count(value => value == (byte)'\n');
        if (source.Length > 0 && source[^1] != (byte)'\n')
            recordCount++;
        string lengths = string.Join(',', first.Select(chunk => chunk.Length));
        int chunkViewSize = Marshal.SizeOf<Native.CChunkView>();
        int dataOffset = Marshal.OffsetOf<Native.CChunkView>("data").ToInt32();
        int lenOffset = Marshal.OffsetOf<Native.CChunkView>("len").ToInt32();
        string result =
            $"abi_version=65539;capabilities=63;partition_count={first.Count};" +
            $"partition_lengths={lengths};total_length={source.Length};record_count={recordCount};" +
            $"fnv1a64={digest:x16};deterministic=1;n0_error={n0Error};" +
            $"chunk_view_size={chunkViewSize};" +
            $"chunk_view_data_offset={dataOffset};" +
            $"chunk_view_len_offset={lenOffset}";
        string expectedResult = File.ReadAllText(expected).Trim();
        if (result != expectedResult)
            Fail($"canonical result mismatch\nexpected: {expectedResult}\nactual:   {result}");
        File.WriteAllText(output, result + Environment.NewLine);
        Console.WriteLine("PASS: C# Linux conformance consumer");
    }

    private static string Required(string[] args, string name)
    {
        int index = Array.IndexOf(args, name);
        if (index < 0 || index + 1 >= args.Length)
            Fail($"missing {name}");
        return args[index + 1];
    }
}
