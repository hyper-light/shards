// What moby/go-archive v0.3.3 and Go's archive/tar make of
// crates/archive/testdata/cases.json, recorded for shards-archive's tests
// (crates/archive/tests/oracle.rs), which hold the Rust port to it:
//
//   - pack: each fixture tree built as root, archived by TarWithOptions or
//     TarResourceRebase; the archive goes to testdata/pack/NAME.tar.
//   - unpack: each input archive made here by Go's tar.Writer goes to
//     testdata/unpack/NAME.tar (others are pack outputs or Go's own test archives), and
//     Unpack's result is recorded: the tree it left (manifest) and its error.
//   - copy: CopyResource between paths of a fixture tree, and the tree it left.
//   - rebase: RebaseArchiveEntries' output, testdata/rebase/NAME.tar.
//   - read: every header and data hash Go's tar.Reader gives of an archive.
//   - paths: SplitPathDirEntry and PreserveTrailingDotOrSeparator.
//
// It runs in scripts/archive/generate's privileged Linux container:
//
//	oracle TESTDATA WORKDIR
package main

import (
	"archive/tar"
	"bytes"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"hash/fnv"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"syscall"
	"time"
	"unicode/utf8"

	"github.com/moby/go-archive"
	"golang.org/x/sys/unix"
)

type entry struct {
	Path   string            `json:"path"`
	Type   string            `json:"type"`
	Mode   *uint32           `json:"mode"`
	UID    *int              `json:"uid"`
	GID    *int              `json:"gid"`
	Data   *string           `json:"data"`
	Size   int               `json:"size"`
	Seed   int               `json:"seed"`
	Target string            `json:"target"`
	Major  uint32            `json:"major"`
	Minor  uint32            `json:"minor"`
	Xattrs map[string]string `json:"xattrs"`
	Mtime  *[2]int64         `json:"mtime"`
}

type packOpts struct {
	Include          []string          `json:"include"`
	Exclude          []string          `json:"exclude"`
	Chown            *[2]int           `json:"chown"`
	IncludeSourceDir bool              `json:"include_source_dir"`
	Rebase           map[string]string `json:"rebase"`
}

type packCase struct {
	Name       string   `json:"name"`
	Tree       string   `json:"tree"`
	Src        string   `json:"src"`
	Kind       string   `json:"kind"`
	RebaseName string   `json:"rebase_name"`
	Opts       packOpts `json:"opts"`
}

type header struct {
	Name     string                     `json:"name"`
	Type     string                     `json:"type"`
	Linkname string                     `json:"linkname"`
	Mode     int64                      `json:"mode"`
	UID      int                        `json:"uid"`
	GID      int                        `json:"gid"`
	Data     *string                    `json:"data"`
	Size     int                        `json:"size"`
	Seed     int                        `json:"seed"`
	Mtime    *[2]int64                  `json:"mtime"`
	Atime    *[2]int64                  `json:"atime"`
	Pax      map[string]json.RawMessage `json:"pax"`
	Format   string                     `json:"format"`
	Devmajor int64                      `json:"devmajor"`
	Devminor int64                      `json:"devminor"`
}

type input struct {
	Entries    []header `json:"entries"`
	TruncateAt int      `json:"truncate_at"`
	Hex        *string  `json:"hex"`
	GoTar      string   `json:"go_tar"`
	Pack       string   `json:"pack"`
	Unpack     string   `json:"unpack"`
}

type unpackOpts struct {
	NoLchown             bool     `json:"no_lchown"`
	Chown                *[2]int  `json:"chown"`
	NoOverwriteDirNonDir bool     `json:"no_overwrite_dir_non_dir"`
	BestEffortXattrs     bool     `json:"best_effort_xattrs"`
	Exclude              []string `json:"exclude"`
}

type unpackCase struct {
	Name   string     `json:"name"`
	Pre    string     `json:"pre"`
	Owners bool       `json:"owners"`
	Opts   unpackOpts `json:"opts"`
	Input  input      `json:"input"`
}

type copyCase struct {
	Name   string `json:"name"`
	Src    string `json:"src"`
	Dst    string `json:"dst"`
	Follow bool   `json:"follow"`
}

type rebaseCase struct {
	Name  string `json:"name"`
	Input input  `json:"input"`
	Old   string `json:"old"`
	New   string `json:"new"`
}

type cases struct {
	Trees  map[string][]entry `json:"trees"`
	Pack   []packCase         `json:"pack"`
	Unpack []unpackCase       `json:"unpack"`
	Copy   []copyCase         `json:"copy"`
	Rebase []rebaseCase       `json:"rebase"`
	Read   []string           `json:"read"`
	Paths  []string           `json:"paths"`
}

// B is a byte string in JSON: as itself when it is UTF-8, else {"hex": ...}.
func B(s string) any {
	if utf8.ValidString(s) {
		return s
	}
	return map[string]string{"hex": hex.EncodeToString([]byte(s))}
}

func pattern(size, seed int) []byte {
	b := make([]byte, size)
	for i := range b {
		b[i] = byte((i*31 + seed*17) % 251)
	}
	return b
}

func fnvHex(b []byte) string {
	h := fnv.New64a()
	h.Write(b)
	return fmt.Sprintf("%016x", h.Sum64())
}

func must(err error) {
	if err != nil {
		panic(err)
	}
}

// build makes a fixture tree: every entry, then owners, modes and attributes, then
// times, deepest first.
func build(root string, entries []entry) {
	must(os.MkdirAll(root, 0o700))
	for _, e := range entries {
		p := filepath.Join(root, e.Path)
		switch e.Type {
		case "dir":
			if e.Path != "" {
				must(os.Mkdir(p, 0o700))
			}
		case "file":
			data := pattern(e.Size, e.Seed)
			if e.Data != nil {
				data = []byte(*e.Data)
			}
			must(os.WriteFile(p, data, 0o600))
		case "symlink":
			must(os.Symlink(e.Target, p))
		case "hardlink":
			must(os.Link(filepath.Join(root, e.Target), p))
		case "fifo":
			must(unix.Mkfifo(p, 0o600))
		case "char":
			must(unix.Mknod(p, unix.S_IFCHR|0o600, int(unix.Mkdev(e.Major, e.Minor))))
		case "block":
			must(unix.Mknod(p, unix.S_IFBLK|0o600, int(unix.Mkdev(e.Major, e.Minor))))
		default:
			panic("type " + e.Type)
		}
	}
	for _, e := range entries {
		if e.Type == "hardlink" {
			continue
		}
		p := filepath.Join(root, e.Path)
		if e.UID != nil || e.GID != nil {
			uid, gid := -1, -1
			if e.UID != nil {
				uid = *e.UID
			}
			if e.GID != nil {
				gid = *e.GID
			}
			must(os.Lchown(p, uid, gid))
		}
		if e.Type != "symlink" && e.Mode != nil {
			must(unix.Fchmodat(unix.AT_FDCWD, p, *e.Mode, 0))
		}
		names := make([]string, 0, len(e.Xattrs))
		for k := range e.Xattrs {
			names = append(names, k)
		}
		sort.Strings(names)
		for _, k := range names {
			v, err := hex.DecodeString(e.Xattrs[k])
			must(err)
			must(unix.Lsetxattr(p, k, v, 0))
		}
	}
	for i := len(entries) - 1; i >= 0; i-- {
		e := entries[i]
		if e.Type == "hardlink" {
			continue
		}
		t := [2]int64{1234567890, 0}
		if e.Mtime != nil {
			t = *e.Mtime
		}
		ts := []unix.Timespec{{Sec: t[0], Nsec: t[1]}, {Sec: t[0], Nsec: t[1]}}
		must(unix.UtimesNanoAt(unix.AT_FDCWD, filepath.Join(root, e.Path), ts, unix.AT_SYMLINK_NOFOLLOW))
	}
}

// manifest is what a tree holds, as the tests compare it: every path under root, never
// following symlinks, its type, mode, owners when asked, data hash, link target, time
// ("now" for one made while unpacking), the extended attributes an archive carries,
// device numbers, and the first path of a hard-linked inode.
func manifest(root string, owners bool, now time.Time) []map[string]any {
	var out []map[string]any
	type ino struct{ dev, ino uint64 }
	first := map[ino]string{}
	err := filepath.WalkDir(root, func(p string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		rel, err := filepath.Rel(root, p)
		if err != nil || rel == "." {
			return err
		}
		var st unix.Stat_t
		if err := unix.Lstat(p, &st); err != nil {
			return err
		}
		m := map[string]any{"path": B(rel)}
		kind := map[uint32]string{
			unix.S_IFDIR: "dir", unix.S_IFREG: "file", unix.S_IFLNK: "symlink",
			unix.S_IFIFO: "fifo", unix.S_IFCHR: "char", unix.S_IFBLK: "block", unix.S_IFSOCK: "socket",
		}[uint32(st.Mode)&unix.S_IFMT]
		m["type"] = kind
		if kind != "symlink" {
			m["mode"] = st.Mode & 0o7777
		}
		if owners {
			m["uid"], m["gid"] = st.Uid, st.Gid
		}
		switch kind {
		case "file":
			data, err := os.ReadFile(p)
			if err != nil {
				return err
			}
			m["size"], m["data"] = len(data), fnvHex(data)
		case "symlink":
			t, err := os.Readlink(p)
			if err != nil {
				return err
			}
			m["target"] = B(t)
		case "char", "block":
			m["major"], m["minor"] = unix.Major(st.Rdev), unix.Minor(st.Rdev)
		}
		if d := now.Unix() - st.Mtim.Sec; d < 86400 && d > -86400 {
			m["mtime"] = "now"
		} else {
			m["mtime"] = [2]int64{st.Mtim.Sec, st.Mtim.Nsec}
		}
		if kind != "dir" && st.Nlink > 1 {
			k := ino{uint64(st.Dev), st.Ino}
			if f, ok := first[k]; ok {
				m["link"] = B(f)
			} else {
				first[k] = rel
			}
		}
		xattrs := map[string]string{}
		sz, err := unix.Llistxattr(p, nil)
		if err == nil && sz > 0 {
			buf := make([]byte, sz)
			sz, err = unix.Llistxattr(p, buf)
			must(err)
			for _, name := range strings.Split(strings.TrimRight(string(buf[:sz]), "\x00"), "\x00") {
				if !strings.HasPrefix(name, "user.") && !strings.HasPrefix(name, "security.") && !strings.HasPrefix(name, "trusted.") {
					continue
				}
				vsz, err := unix.Lgetxattr(p, name, nil)
				must(err)
				v := make([]byte, vsz)
				vsz, err = unix.Lgetxattr(p, name, v)
				must(err)
				xattrs[name] = hex.EncodeToString(v[:vsz])
			}
		}
		if len(xattrs) > 0 {
			m["xattrs"] = xattrs
		}
		out = append(out, m)
		return nil
	})
	must(err)
	return out
}

func makeTar(in input, testdata string, packs map[string][]byte, inputs map[string][]byte) []byte {
	switch {
	case in.Hex != nil:
		b, err := hex.DecodeString(*in.Hex)
		must(err)
		return b
	case in.GoTar != "":
		b, err := os.ReadFile(filepath.Join(testdata, "go-tar", in.GoTar))
		must(err)
		return b
	case in.Pack != "":
		return packs[in.Pack]
	case in.Unpack != "":
		return inputs[in.Unpack]
	}
	var buf bytes.Buffer
	tw := tar.NewWriter(&buf)
	for _, h := range in.Entries {
		data := pattern(h.Size, h.Seed)
		if h.Data != nil {
			data = []byte(*h.Data)
		}
		hdr := &tar.Header{
			Typeflag: h.Type[0],
			Name:     h.Name,
			Linkname: h.Linkname,
			Mode:     h.Mode,
			Uid:      h.UID,
			Gid:      h.GID,
			Size:     int64(len(data)),
			Devmajor: h.Devmajor,
			Devminor: h.Devminor,
			ModTime:  time.Unix(1234567890, 0),
		}
		if h.Mtime != nil {
			hdr.ModTime = time.Unix(h.Mtime[0], h.Mtime[1])
		}
		if h.Atime != nil {
			hdr.AccessTime = time.Unix(h.Atime[0], h.Atime[1])
		}
		switch hdr.Typeflag {
		case tar.TypeLink, tar.TypeSymlink, tar.TypeChar, tar.TypeBlock, tar.TypeDir, tar.TypeFifo, tar.TypeXGlobalHeader:
			hdr.Size, data = 0, nil
		}
		if hdr.Typeflag == tar.TypeXGlobalHeader {
			hdr.ModTime = time.Time{}
		}
		switch h.Format {
		case "pax":
			hdr.Format = tar.FormatPAX
		case "gnu":
			hdr.Format = tar.FormatGNU
		case "ustar":
			hdr.Format = tar.FormatUSTAR
		}
		if len(h.Pax) > 0 {
			hdr.PAXRecords = map[string]string{}
			for k, raw := range h.Pax {
				var s string
				if json.Unmarshal(raw, &s) != nil {
					var hx struct{ Hex string }
					must(json.Unmarshal(raw, &hx))
					b, err := hex.DecodeString(hx.Hex)
					must(err)
					s = string(b)
				}
				hdr.PAXRecords[k] = s
			}
		}
		must(tw.WriteHeader(hdr))
		_, err := tw.Write(data)
		must(err)
	}
	must(tw.Close())
	b := buf.Bytes()
	if in.TruncateAt > 0 {
		b = b[:in.TruncateAt]
	}
	return b
}

// dump is what Go's tar.Reader gives of an archive.
func dump(b []byte) map[string]any {
	tr := tar.NewReader(bytes.NewReader(b))
	var entries []map[string]any
	t := func(t time.Time) any {
		if t.IsZero() {
			return nil
		}
		return [2]int64{t.Unix(), int64(t.Nanosecond())}
	}
	for {
		h, err := tr.Next()
		if err == io.EOF {
			return map[string]any{"entries": entries}
		}
		if err != nil {
			return map[string]any{"entries": entries, "error": err.Error()}
		}
		var pax [][2]any
		keys := make([]string, 0, len(h.PAXRecords))
		for k := range h.PAXRecords {
			keys = append(keys, k)
		}
		sort.Strings(keys)
		for _, k := range keys {
			pax = append(pax, [2]any{B(k), B(h.PAXRecords[k])})
		}
		data, rerr := io.ReadAll(tr)
		e := map[string]any{
			"typeflag": h.Typeflag, "name": B(h.Name), "linkname": B(h.Linkname), "size": h.Size,
			"mode": h.Mode, "uid": h.Uid, "gid": h.Gid, "uname": B(h.Uname), "gname": B(h.Gname),
			"mtime": t(h.ModTime), "atime": t(h.AccessTime), "ctime": t(h.ChangeTime),
			"devmajor": h.Devmajor, "devminor": h.Devminor, "pax": pax, "format": int(h.Format),
			"data": fnvHex(data), "data_len": len(data),
		}
		if rerr != nil {
			e["error"] = rerr.Error()
		}
		entries = append(entries, e)
		if rerr != nil {
			return map[string]any{"entries": entries}
		}
	}
}

func errText(err error, path, as string) any {
	if err == nil {
		return nil
	}
	return strings.ReplaceAll(err.Error(), path, as)
}

func main() {
	testdata, work := os.Args[1], os.Args[2]
	syscall.Umask(0o022)
	raw, err := os.ReadFile(filepath.Join(testdata, "cases.json"))
	must(err)
	var c cases
	must(json.Unmarshal(raw, &c))
	for _, dir := range []string{"pack", "unpack", "rebase"} {
		must(os.RemoveAll(filepath.Join(testdata, dir)))
		must(os.MkdirAll(filepath.Join(testdata, dir), 0o755))
	}
	answers := map[string]any{}
	now := time.Now()

	trees := map[string]string{}
	tree := func(name string) string {
		if p, ok := trees[name]; ok {
			return p
		}
		p := filepath.Join(work, "tree-"+name)
		build(p, c.Trees[name])
		trees[name] = p
		return p
	}

	packs := map[string][]byte{}
	packAnswers := map[string]any{}
	for _, pc := range c.Pack {
		root := tree(pc.Tree)
		src := root
		if pc.Src != "" {
			src = root + "/" + pc.Src
		}
		var rc io.ReadCloser
		if pc.Kind == "resource" {
			rc, err = archive.TarResourceRebase(src, pc.RebaseName)
		} else {
			opts := &archive.TarOptions{
				IncludeFiles:     pc.Opts.Include,
				ExcludePatterns:  pc.Opts.Exclude,
				IncludeSourceDir: pc.Opts.IncludeSourceDir,
				RebaseNames:      pc.Opts.Rebase,
			}
			if pc.Opts.Chown != nil {
				opts.ChownOpts = &archive.ChownOpts{UID: pc.Opts.Chown[0], GID: pc.Opts.Chown[1]}
			}
			rc, err = archive.TarWithOptions(src, opts)
		}
		if err != nil {
			packAnswers[pc.Name] = map[string]any{"error": errText(err, root, "<root>")}
			continue
		}
		b, err := io.ReadAll(rc)
		must(err)
		packs[pc.Name] = b
		must(os.WriteFile(filepath.Join(testdata, "pack", pc.Name+".tar"), b, 0o644))
		packAnswers[pc.Name] = map[string]any{"error": nil}
	}
	answers["pack"] = packAnswers

	inputs := map[string][]byte{}
	unpackAnswers := map[string]any{}
	for _, uc := range c.Unpack {
		b := makeTar(uc.Input, testdata, packs, inputs)
		inputs[uc.Name] = b
		if uc.Input.GoTar == "" && uc.Input.Pack == "" {
			must(os.WriteFile(filepath.Join(testdata, "unpack", uc.Name+".tar"), b, 0o644))
		}
		dest := filepath.Join(work, "unpack-"+uc.Name, "dest")
		if uc.Pre != "" {
			build(dest, c.Trees[uc.Pre])
		} else {
			must(os.MkdirAll(dest, 0o755))
		}
		opts := &archive.TarOptions{
			NoLchown:             uc.Opts.NoLchown,
			NoOverwriteDirNonDir: uc.Opts.NoOverwriteDirNonDir,
			BestEffortXattrs:     uc.Opts.BestEffortXattrs,
			ExcludePatterns:      uc.Opts.Exclude,
		}
		if uc.Opts.Chown != nil {
			opts.ChownOpts = &archive.ChownOpts{UID: uc.Opts.Chown[0], GID: uc.Opts.Chown[1]}
		}
		err := archive.Unpack(bytes.NewReader(b), dest, opts)
		unpackAnswers[uc.Name] = map[string]any{
			"error":    errText(err, dest, "<dest>"),
			"manifest": manifest(filepath.Dir(dest), uc.Owners, now),
		}
	}
	answers["unpack"] = unpackAnswers

	copyAnswers := map[string]any{}
	for _, cc := range c.Copy {
		root := filepath.Join(work, "copy-"+cc.Name)
		build(root, c.Trees["copy"])
		err := archive.CopyResource(root+"/"+cc.Src, root+"/"+cc.Dst, cc.Follow)
		copyAnswers[cc.Name] = map[string]any{
			"error":    errText(err, root, "<root>"),
			"manifest": manifest(root, false, now),
		}
	}
	answers["copy"] = copyAnswers

	rebaseAnswers := map[string]any{}
	for _, rc := range c.Rebase {
		in := makeTar(rc.Input, testdata, packs, inputs)
		out, err := io.ReadAll(archive.RebaseArchiveEntries(bytes.NewReader(in), rc.Old, rc.New))
		must(os.WriteFile(filepath.Join(testdata, "rebase", rc.Name+".tar"), out, 0o644))
		rebaseAnswers[rc.Name] = map[string]any{"error": errText(err, "", "")}
	}
	answers["rebase"] = rebaseAnswers

	reads := map[string]any{}
	for _, name := range c.Read {
		b, err := os.ReadFile(filepath.Join(testdata, "go-tar", name))
		must(err)
		reads["go-tar/"+name] = dump(b)
	}
	for name, b := range packs {
		reads["pack/"+name+".tar"] = dump(b)
	}
	for _, uc := range c.Unpack {
		if uc.Input.GoTar == "" && uc.Input.Pack == "" {
			reads["unpack/"+uc.Name+".tar"] = dump(inputs[uc.Name])
		}
	}
	answers["read"] = reads

	var paths []any
	for _, p := range c.Paths {
		dir, base := archive.SplitPathDirEntry(p)
		paths = append(paths, map[string]any{
			"path":     p,
			"split":    [2]string{dir, base},
			"preserve": archive.PreserveTrailingDotOrSeparator(filepath.Clean(p), p),
		})
	}
	answers["paths"] = paths

	out, err := json.MarshalIndent(answers, "", " ")
	must(err)
	must(os.WriteFile(filepath.Join(testdata, "answers.json"), append(out, '\n'), 0o644))
}
