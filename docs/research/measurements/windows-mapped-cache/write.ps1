# Writes FILE, MIB MiB of random data, past the cache, which holds none of it (PM M421).
#
#   powershell -NoProfile -File write.ps1 FILE MIB
param([Parameter(Mandatory)][string]$File, [Parameter(Mandatory)][int]$MiB)
$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @"
using System;
using System.IO;
public static class Writing {
    public static void Write(string path, int mib) {
        var buf = new byte[1 << 20];
        new Random(7).NextBytes(buf);
        using (var f = new FileStream(path, FileMode.CreateNew, FileAccess.Write, FileShare.None, 4096, (FileOptions)0x20000000 | FileOptions.WriteThrough)) {
            for (int i = 0; i < mib; i++) f.Write(buf, 0, buf.Length);
        }
    }
}
"@
[Writing]::Write($File, $MiB)
