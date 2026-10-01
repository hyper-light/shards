// What buildx's client sends of a build context, for crates/build/testdata/contexts.json:
// the fixture tree made in a temporary directory, then walked as buildx walks a local
// source (BuildKit's client/solve.go prepareSyncedFiles, owners reset to root, and
// session/filesync's filter of the include, exclude and follow paths asked for),
// with fsutil as BuildKit dockerfile/1.27.1 vendors it. Writes contexts-answers.json.
package main

import (
	"context"
	"encoding/json"
	"fmt"
	"io/fs"
	"net"
	"os"
	"path/filepath"

	"github.com/tonistiigi/fsutil"
	"github.com/tonistiigi/fsutil/types"
	"golang.org/x/sys/unix"
)

type entry struct {
	Path   string    `json:"path"`
	Type   string    `json:"type"`
	Mode   *uint32   `json:"mode"`
	Data   string    `json:"data"`
	Target string    `json:"target"`
	Mtime  *[2]int64 `json:"mtime"`
}

type filterCase struct {
	Name    string   `json:"name"`
	Include []string `json:"include"`
	Exclude []string `json:"exclude"`
	Follow  []string `json:"follow"`
}

type spec struct {
	Tree  []entry      `json:"tree"`
	Cases []filterCase `json:"cases"`
}

func must(err error) {
	if err != nil {
		panic(err)
	}
}

func times(p string, tm [2]int64) {
	ts := []unix.Timespec{{Sec: tm[0], Nsec: tm[1]}, {Sec: tm[0], Nsec: tm[1]}}
	must(unix.UtimesNanoAt(unix.AT_FDCWD, p, ts, unix.AT_SYMLINK_NOFOLLOW))
}

func main() {
	data, err := os.ReadFile(os.Args[1])
	must(err)
	var s spec
	must(json.Unmarshal(data, &s))
	root, err := os.MkdirTemp("", "shards-context")
	must(err)
	defer os.RemoveAll(root)
	var dirs []entry
	for _, e := range s.Tree {
		p := filepath.Join(root, e.Path)
		tm := [2]int64{1600000000, 500}
		if e.Mtime != nil {
			tm = *e.Mtime
		}
		switch e.Type {
		case "dir":
			must(os.Mkdir(p, 0700))
			dirs = append(dirs, e)
		case "file":
			must(os.WriteFile(p, []byte(e.Data), 0600))
		case "symlink":
			must(os.Symlink(e.Target, p))
		case "hardlink":
			must(os.Link(filepath.Join(root, e.Target), p))
			continue
		case "fifo":
			must(unix.Mkfifo(p, 0600))
		case "socket":
			l, err := net.Listen("unix", p)
			must(err)
			l.(*net.UnixListener).SetUnlinkOnClose(false)
			must(l.Close())
		default:
			panic(e.Type)
		}
		if e.Type != "symlink" {
			mode := uint32(0644)
			if e.Type == "dir" {
				mode = 0755
			}
			if e.Mode != nil {
				mode = *e.Mode
			}
			must(unix.Chmod(p, mode))
		}
		if e.Type != "dir" {
			times(p, tm)
		}
	}
	for i := len(dirs) - 1; i >= 0; i-- {
		tm := [2]int64{1600000000, 500}
		if dirs[i].Mtime != nil {
			tm = *dirs[i].Mtime
		}
		times(filepath.Join(root, dirs[i].Path), tm)
	}

	reset := func(p string, st *types.Stat) fsutil.MapResult {
		st.Uid = 0
		st.Gid = 0
		return fsutil.MapResultKeep
	}
	var out []map[string]any
	for _, c := range s.Cases {
		r := map[string]any{"name": c.Name}
		sent, err := func() ([]map[string]any, error) {
			f, err := fsutil.NewFS(root)
			if err != nil {
				return nil, err
			}
			f, err = fsutil.NewFilterFS(f, &fsutil.FilterOpt{Map: reset})
			if err != nil {
				return nil, err
			}
			f, err = fsutil.NewFilterFS(f, &fsutil.FilterOpt{
				IncludePatterns: c.Include,
				ExcludePatterns: c.Exclude,
				FollowPaths:     c.Follow,
			})
			if err != nil {
				return nil, err
			}
			var sent []map[string]any
			err = f.Walk(context.Background(), "/", func(p string, d fs.DirEntry, err error) error {
				if err != nil {
					return err
				}
				fi, err := d.Info()
				if err != nil {
					return err
				}
				st := fi.Sys().(*types.Stat)
				sent = append(sent, map[string]any{
					"path": st.Path, "mode": st.Mode, "size": st.Size,
					"link": st.Linkname, "mtime": st.ModTime,
				})
				return nil
			})
			return sent, err
		}()
		if err != nil {
			r["error"] = fmt.Sprint(err)
		} else {
			r["sent"] = sent
		}
		out = append(out, r)
	}
	res, err := json.MarshalIndent(out, "", " ")
	must(err)
	must(os.WriteFile(os.Args[2], append(res, '\n'), 0644))
}
