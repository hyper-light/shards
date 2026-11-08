# Holds FILE open until killed: with VIEW, a read-only view of it mapped too, its pages
# between FROM and TO read once, so that the view maps them (PM M421).
#
#   powershell -NoProfile -File hold.ps1 FILE [-View] [-From FROM -To TO]
param([Parameter(Mandatory)][string]$File, [switch]$View, [long]$From = 0, [long]$To = 0)
$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @"
using System;
using System.IO;
using System.IO.MemoryMappedFiles;
using System.Threading;
public static class Holding {
    public static void Hold(string path, bool view, long from, long to) {
        using (var f = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete)) {
            if (!view) { Thread.Sleep(Timeout.Infinite); }
            using (var map = MemoryMappedFile.CreateFromFile(f, null, 0, MemoryMappedFileAccess.Read, HandleInheritability.None, true))
            using (var v = map.CreateViewAccessor(0, 0, MemoryMappedFileAccess.Read)) {
                int sum = 0;
                for (long at = from; at < to; at += 4096) sum += v.ReadByte(at);
                Console.WriteLine("held " + sum);
                Thread.Sleep(Timeout.Infinite);
            }
        }
    }
}
"@
[Holding]::Hold($File, [bool]$View, $From, $To)
