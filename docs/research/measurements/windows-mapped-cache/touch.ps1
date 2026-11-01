# Touches PAGES random pages of FILE, between byte offsets FROM and TO (the whole file
# where TO is 0), through a copy-on-write view, as probe.ps1 does, in a process of its own:
# the milliseconds taken (PM M421).
#
#   powershell -NoProfile -File touch.ps1 FILE [PAGES [FROM TO]]
param([Parameter(Mandatory)][string]$File, [int]$Pages = 1000, [long]$From = 0, [long]$To = 0)
$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @"
using System;
using System.Diagnostics;
using System.IO;
using System.IO.MemoryMappedFiles;
public static class Touching {
    public static double Touch(string path, int pages, long from, long to) {
        using (var f = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete))
        using (var map = MemoryMappedFile.CreateFromFile(f, null, 0, MemoryMappedFileAccess.CopyOnWrite, HandleInheritability.None, true))
        using (var view = map.CreateViewAccessor(0, 0, MemoryMappedFileAccess.CopyOnWrite)) {
            if (to == 0) to = f.Length;
            long n = (to - from) / 4096;
            var rng = new Random(1);
            var at = new long[pages];
            for (int i = 0; i < pages; i++) at[i] = from + (long)(rng.NextDouble() * n) * 4096;
            var clock = Stopwatch.StartNew();
            int sum = 0;
            for (int i = 0; i < pages; i++) sum += view.ReadByte(at[i]);
            clock.Stop();
            return clock.Elapsed.TotalMilliseconds + (sum < 0 ? 1 : 0);
        }
    }
}
"@
"{0:N1}" -f [Touching]::Touch($File, $Pages, $From, $To)
