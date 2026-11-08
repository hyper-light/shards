//go:build !windows

package main

import "golang.org/x/sys/unix"

// times sets the path's own access and modification times, a symlink not followed.
func times(p string, tm [2]int64) {
	ts := []unix.Timespec{{Sec: tm[0], Nsec: tm[1]}, {Sec: tm[0], Nsec: tm[1]}}
	must(unix.UtimesNanoAt(unix.AT_FDCWD, p, ts, unix.AT_SYMLINK_NOFOLLOW))
}

// chmod sets the mode's bits as chmod(2) takes them, set-ID and sticky bits included.
func chmod(p string, mode uint32) error {
	return unix.Chmod(p, mode)
}

// mkfifo makes a FIFO at p.
func mkfifo(p string) bool {
	must(unix.Mkfifo(p, 0600))
	return true
}
