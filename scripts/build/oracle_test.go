package file

// What BuildKit makes of crates/build/testdata/ops.json: each case's actions run by
// BuildKit's own file backend (mkdir, mkfile, docopy, and ops/user_linux.go's readUser,
// copied beside this file by scripts/build/generate) on an overlayfs mount of the case's
// lower tree, as root, and the layer BuildKit's overlay differ writes of the result
// (util/overlay WriteUpperdir, containerd's ChangeWriter and Go's archive/tar). A
// "remove" action, os.RemoveAll, is not BuildKit's: it makes the whiteouts and opaque
// directories the differ writes. Nor is "lchown", which shows what chown clears.
//
// Times the kernel stamps while the actions run are moved to the sentinel before the diff,
// so the answers do not depend on when they were made; shards' tests run with that time
// as now.

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"testing"
	"time"

	"github.com/containerd/containerd/v2/core/mount"
	"github.com/moby/buildkit/solver/pb"
	"github.com/moby/buildkit/util/overlay"
	"github.com/moby/sys/user"
	"github.com/tonistiigi/fsutil"
	copy "github.com/tonistiigi/fsutil/copy"
	"github.com/tonistiigi/fsutil/types"
	"golang.org/x/sys/unix"
)

const sentinelSec, sentinelNsec = 2000000000, 123456789

type entry struct {
	Path   string            `json:"path"`
	Type   string            `json:"type"`
	Mode   *uint32           `json:"mode"`
	UID    int               `json:"uid"`
	GID    int               `json:"gid"`
	Mtime  *[2]int64         `json:"mtime"`
	Data   string            `json:"data"`
	Target string            `json:"target"`
	Major  uint32            `json:"major"`
	Minor  uint32            `json:"minor"`
	Xattrs map[string]string `json:"xattrs"`
}

type userOpt struct {
	ID   *uint32 `json:"id"`
	Name string  `json:"name"`
}

type owner struct {
	User  *userOpt `json:"user"`
	Group *userOpt `json:"group"`
}

type action struct {
	Kind               string   `json:"kind"`
	Path               string   `json:"path"`
	Mode               int32    `json:"mode"`
	Parents            bool     `json:"parents"`
	Data               string   `json:"data"`
	Timestamp          int64    `json:"timestamp"`
	Owner              *owner   `json:"owner"`
	Src                string   `json:"src"`
	Dest               string   `json:"dest"`
	ModeStr            string   `json:"mode_str"`
	FollowSymlink      bool     `json:"follow_symlink"`
	DirCopyContents    bool     `json:"dir_copy_contents"`
	CreateDestPath     bool     `json:"create_dest_path"`
	AllowWildcard      bool     `json:"allow_wildcard"`
	AllowEmptyWildcard bool     `json:"allow_empty_wildcard"`
	Include            []string `json:"include"`
	UID                int      `json:"uid"`
	GID                int      `json:"gid"`
	Exclude            []string `json:"exclude"`
}

type testCase struct {
	Name    string   `json:"name"`
	Lower   []entry  `json:"lower"`
	Src     []entry  `json:"src"`
	Actions []action `json:"actions"`
	// Context makes the source a build context: Src is sent by fsutil's sender, owners
	// reset as buildx resets them, and written by its receiver, as a local source is.
	Context bool `json:"context"`
}

// One end of an in-memory stream of fsutil packets.
type pipeEnd struct {
	ctx context.Context
	in  <-chan []byte
	out chan<- []byte
}

func (p *pipeEnd) Context() context.Context { return p.ctx }

func (p *pipeEnd) SendMsg(m any) error {
	dt, err := m.(*types.Packet).MarshalVT()
	if err != nil {
		return err
	}
	select {
	case p.out <- dt:
		return nil
	case <-p.ctx.Done():
		return p.ctx.Err()
	}
}

func (p *pipeEnd) RecvMsg(m any) error {
	select {
	case dt, ok := <-p.in:
		if !ok {
			return io.EOF
		}
		return m.(*types.Packet).UnmarshalVT(dt)
	case <-p.ctx.Done():
		return p.ctx.Err()
	}
}

// sendReceive sends dir as buildx sends a context and receives it into dest.
func sendReceive(dir, dest string) error {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	a2b, b2a := make(chan []byte, 64), make(chan []byte, 64)
	a := &pipeEnd{ctx: ctx, in: b2a, out: a2b}
	b := &pipeEnd{ctx: ctx, in: a2b, out: b2a}
	f, err := fsutil.NewFS(dir)
	if err != nil {
		return err
	}
	f, err = fsutil.NewFilterFS(f, &fsutil.FilterOpt{Map: func(_ string, st *types.Stat) fsutil.MapResult {
		st.Uid = 0
		st.Gid = 0
		return fsutil.MapResultKeep
	}})
	if err != nil {
		return err
	}
	f, err = fsutil.NewFilterFS(f, &fsutil.FilterOpt{})
	if err != nil {
		return err
	}
	sent := make(chan error, 1)
	go func() {
		err := fsutil.Send(ctx, a, f, nil)
		close(a2b)
		sent <- err
	}()
	if err := fsutil.Receive(ctx, b, dest, fsutil.ReceiveOpt{}); err != nil {
		cancel()
		<-sent
		return err
	}
	return <-sent
}

type answer struct {
	Name  string `json:"name"`
	Error string `json:"error"`
	Layer string `json:"layer"`
}

// xattr values are text, or base64 after "base64:".
func xattrValue(v string) []byte {
	if rest, ok := strings.CutPrefix(v, "base64:"); ok {
		b, err := base64.StdEncoding.DecodeString(rest)
		if err != nil {
			panic(err)
		}
		return b
	}
	return []byte(v)
}

func materialize(t *testing.T, root string, entries []entry) {
	type dirTime struct {
		p  string
		tm [2]int64
	}
	var dirs []dirTime
	for _, e := range entries {
		p := filepath.Join(root, e.Path)
		tm := [2]int64{1600000000, 500}
		if e.Mtime != nil {
			tm = *e.Mtime
		}
		mode := func(def uint32) uint32 {
			if e.Mode != nil {
				return *e.Mode
			}
			return def
		}
		var err error
		switch e.Type {
		case "dir":
			err = os.Mkdir(p, 0700)
			dirs = append(dirs, dirTime{p, tm})
		case "file":
			err = os.WriteFile(p, []byte(e.Data), 0600)
		case "symlink":
			err = os.Symlink(e.Target, p)
		case "hardlink":
			err = os.Link(filepath.Join(root, e.Target), p)
		case "char":
			err = unix.Mknod(p, unix.S_IFCHR|0600, int(unix.Mkdev(e.Major, e.Minor)))
		case "block":
			err = unix.Mknod(p, unix.S_IFBLK|0600, int(unix.Mkdev(e.Major, e.Minor)))
		case "fifo":
			err = unix.Mkfifo(p, 0600)
		default:
			t.Fatalf("%s: unknown type %q", e.Path, e.Type)
		}
		if err != nil {
			t.Fatal(err)
		}
		if e.Type == "hardlink" {
			continue
		}
		if err := os.Lchown(p, e.UID, e.GID); err != nil {
			t.Fatal(err)
		}
		if e.Type != "symlink" {
			def := uint32(0644)
			if e.Type == "dir" {
				def = 0755
			}
			if err := unix.Chmod(p, mode(def)); err != nil {
				t.Fatal(err)
			}
		}
		keys := make([]string, 0, len(e.Xattrs))
		for k := range e.Xattrs {
			keys = append(keys, k)
		}
		sort.Strings(keys)
		for _, k := range keys {
			if err := unix.Lsetxattr(p, k, xattrValue(e.Xattrs[k]), 0); err != nil {
				t.Fatalf("%s: %s: %v", e.Path, k, err)
			}
		}
		if e.Type != "dir" {
			ts := []unix.Timespec{{Sec: tm[0], Nsec: tm[1]}, {Sec: tm[0], Nsec: tm[1]}}
			if err := unix.UtimesNanoAt(unix.AT_FDCWD, p, ts, unix.AT_SYMLINK_NOFOLLOW); err != nil {
				t.Fatal(err)
			}
		}
	}
	for i := len(dirs) - 1; i >= 0; i-- {
		d := dirs[i]
		ts := []unix.Timespec{{Sec: d.tm[0], Nsec: d.tm[1]}, {Sec: d.tm[0], Nsec: d.tm[1]}}
		if err := unix.UtimesNanoAt(unix.AT_FDCWD, d.p, ts, unix.AT_SYMLINK_NOFOLLOW); err != nil {
			t.Fatal(err)
		}
	}
	ts := []unix.Timespec{{Sec: 1600000000, Nsec: 500}, {Sec: 1600000000, Nsec: 500}}
	if err := unix.UtimesNanoAt(unix.AT_FDCWD, root, ts, 0); err != nil {
		t.Fatal(err)
	}
}

type dirMountable string

func (d dirMountable) Mount() ([]mount.Mount, func() error, error) {
	return []mount.Mount{{Type: "bind", Source: string(d), Options: []string{"rbind"}}}, func() error { return nil }, nil
}

func (d dirMountable) IdentityMapping() *user.IdentityMapping { return nil }

func chownOpt(o *owner) *pb.ChownOpt {
	if o == nil {
		return nil
	}
	conv := func(u *userOpt) *pb.UserOpt {
		if u == nil {
			return nil
		}
		if u.ID != nil {
			return &pb.UserOpt{User: &pb.UserOpt_ByID{ByID: *u.ID}}
		}
		return &pb.UserOpt{User: &pb.UserOpt_ByName{ByName: &pb.NamedUserOpt{Name: u.Name}}}
	}
	return &pb.ChownOpt{User: conv(o.User), Group: conv(o.Group)}
}

func run(t *testing.T, c testCase, work string) answer {
	lower := filepath.Join(work, "lower")
	upper := filepath.Join(work, "upper")
	ovlWork := filepath.Join(work, "work")
	merged := filepath.Join(work, "merged")
	src := filepath.Join(work, "src")
	for _, d := range []string{lower, upper, ovlWork, merged, src} {
		if err := os.MkdirAll(d, 0755); err != nil {
			t.Fatal(err)
		}
	}
	materialize(t, lower, c.Lower)
	if c.Context {
		ctxDir := filepath.Join(work, "ctx")
		if err := os.MkdirAll(ctxDir, 0755); err != nil {
			t.Fatal(err)
		}
		materialize(t, ctxDir, c.Src)
		if err := sendReceive(ctxDir, src); err != nil {
			t.Fatalf("%s: %v", c.Name, err)
		}
	} else {
		materialize(t, src, c.Src)
	}
	opts := "lowerdir=" + lower + ",upperdir=" + upper + ",workdir=" + ovlWork + ",index=off"
	if err := unix.Mount("overlay", merged, "overlay", 0, opts); err != nil {
		t.Fatal(err)
	}
	// The kernel stamps with its coarse clock, which may lag time.Now by a tick: what was
	// stamped in the last hour was stamped by the actions, as fixtures are years older.
	start := time.Now().Add(-time.Hour)
	var actErr error
	for _, a := range c.Actions {
		chopt := chownOpt(a.Owner)
		var u *copy.User
		if chopt != nil {
			u, actErr = readUser(chopt, dirMountable(lower), dirMountable(lower))
			if actErr != nil {
				break
			}
		}
		switch a.Kind {
		case "mkdir":
			actErr = mkdir(merged, &pb.FileActionMkDir{Path: a.Path, Mode: a.Mode, MakeParents: a.Parents, Timestamp: a.Timestamp}, u, nil)
		case "mkfile":
			actErr = mkfile(merged, &pb.FileActionMkFile{Path: a.Path, Mode: a.Mode, Data: []byte(a.Data), Timestamp: a.Timestamp}, u, nil)
		case "copy":
			actErr = docopy(context.Background(), src, merged, &pb.FileActionCopy{
				Src: a.Src, Dest: a.Dest, Mode: a.Mode, ModeStr: a.ModeStr,
				FollowSymlink: a.FollowSymlink, DirCopyContents: a.DirCopyContents,
				CreateDestPath: a.CreateDestPath, AllowWildcard: a.AllowWildcard,
				AllowEmptyWildcard: a.AllowEmptyWildcard, Timestamp: a.Timestamp,
				IncludePatterns: a.Include, ExcludePatterns: a.Exclude,
			}, u, nil)
		case "remove":
			// Not an action of BuildKit's: a deletion, for the differ's whiteouts.
			actErr = os.RemoveAll(filepath.Join(merged, a.Path))
		case "lchown":
			// Not an action of BuildKit's either: what chown does to modes and xattrs.
			actErr = os.Lchown(filepath.Join(merged, a.Path), a.UID, a.GID)
		default:
			t.Fatalf("%s: unknown action %q", c.Name, a.Kind)
		}
		if actErr != nil {
			break
		}
	}
	if err := unix.Unmount(merged, 0); err != nil {
		t.Fatal(err)
	}
	ans := answer{Name: c.Name}
	if actErr != nil {
		msg := actErr.Error()
		for _, root := range []string{merged, src, lower} {
			msg = strings.ReplaceAll(msg, root, "")
		}
		ans.Error = msg
		return ans
	}
	// Times stamped by the kernel since the actions began become the sentinel.
	err := filepath.Walk(upper, func(p string, fi os.FileInfo, err error) error {
		if err != nil {
			return err
		}
		if fi.ModTime().Before(start) {
			return nil
		}
		ts := []unix.Timespec{{Sec: sentinelSec, Nsec: sentinelNsec}, {Sec: sentinelSec, Nsec: sentinelNsec}}
		return unix.UtimesNanoAt(unix.AT_FDCWD, p, ts, unix.AT_SYMLINK_NOFOLLOW)
	})
	if err != nil {
		t.Fatal(err)
	}
	var buf bytes.Buffer
	lowerMounts := []mount.Mount{{Type: "bind", Source: lower, Options: []string{"rbind", "ro"}}}
	if err := overlay.WriteUpperdir(context.Background(), &buf, upper, lowerMounts); err != nil {
		t.Fatalf("%s: %v", c.Name, err)
	}
	ans.Layer = base64.StdEncoding.EncodeToString(buf.Bytes())
	return ans
}

func TestShardsOracle(t *testing.T) {
	in, out := os.Getenv("SHARDS_OPS"), os.Getenv("SHARDS_OPS_OUT")
	if in == "" {
		t.Skip("SHARDS_OPS is not set")
	}
	dt, err := os.ReadFile(in)
	if err != nil {
		t.Fatal(err)
	}
	var cases []testCase
	if err := json.Unmarshal(dt, &cases); err != nil {
		t.Fatal(err)
	}
	unix.Umask(0022)
	var answers []answer
	for i, c := range cases {
		work := filepath.Join(os.Getenv("SHARDS_WORK"), fmt.Sprintf("case%d", i))
		answers = append(answers, run(t, c, work))
		os.RemoveAll(work)
	}
	res, err := json.MarshalIndent(answers, "", " ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(res, '\n'), 0644); err != nil {
		t.Fatal(err)
	}
}
