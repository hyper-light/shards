//go:build windows

package main

import (
	"os"

	"golang.org/x/sys/windows"
)

// times sets the path's own last access and last write times, a reparse point (a symlink,
// an AF_UNIX socket's file) not followed. NTFS keeps 100 ns: NsecToFiletime truncates.
func times(p string, tm [2]int64) {
	name, err := windows.UTF16PtrFromString(p)
	must(err)
	h, err := windows.CreateFile(name, windows.FILE_WRITE_ATTRIBUTES,
		windows.FILE_SHARE_READ|windows.FILE_SHARE_WRITE|windows.FILE_SHARE_DELETE, nil,
		windows.OPEN_EXISTING, windows.FILE_FLAG_BACKUP_SEMANTICS|windows.FILE_FLAG_OPEN_REPARSE_POINT, 0)
	must(err)
	defer windows.CloseHandle(h)
	ft := windows.NsecToFiletime(tm[0]*1_000_000_000 + tm[1])
	must(windows.SetFileTime(h, nil, &ft, &ft))
}

// chmod sets what NTFS keeps of a mode, read-only where the owner may not write
// (os.Chmod, syscall.Chmod).
func chmod(p string, mode uint32) error {
	return os.Chmod(p, os.FileMode(mode&0o777))
}

// mkfifo makes nothing: Windows keeps named pipes in the named-pipe file system
// (\\.\pipe\), never in a directory, so the tree's FIFO is left out of Windows' answers.
func mkfifo(string) bool {
	return false
}
