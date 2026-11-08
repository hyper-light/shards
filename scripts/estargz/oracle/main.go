// What stargz-snapshotter's estargz writer (v0.18.2, as BuildKit v0.28.1 vendors and
// drives it: estargz.NewWriterLevel, AppendTarLossLess, Close) makes of tars that
// crates/shards/src/build/estargz.rs's tests build again byte for byte: each tar's
// digest, and for each level the blob's length and digest, the TOC's digest, the DiffID
// and the uncompressed size (D82).
package main

import (
	"archive/tar"
	"bytes"
	"compress/gzip"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"io"
	"os"
	"time"

	"github.com/containerd/stargz-snapshotter/estargz"
)

type lcg struct{ s uint64 }

func (g *lcg) next() byte {
	g.s = g.s*6364136223846793005 + 1442695040888963407
	return byte(g.s >> 56)
}

// content: n bytes, text-like for the first half, random for the rest.
func content(seed uint64, n int) []byte {
	g := &lcg{seed}
	b := make([]byte, n)
	for i := range b {
		if i < n/2 {
			b[i] = "the quick brown fox jumps over a lazy dog\n"[int(g.next())%42]
		} else {
			b[i] = g.next()
		}
	}
	return b
}

type entry struct {
	hdr  tar.Header
	data []byte
}

func build(entries []entry) []byte {
	var buf bytes.Buffer
	tw := tar.NewWriter(&buf)
	for _, e := range entries {
		h := e.hdr
		if h.Typeflag == tar.TypeReg {
			h.Size = int64(len(e.data))
		}
		if err := tw.WriteHeader(&h); err != nil {
			panic(err)
		}
		if _, err := tw.Write(e.data); err != nil {
			panic(err)
		}
	}
	if err := tw.Close(); err != nil {
		panic(err)
	}
	return buf.Bytes()
}

func tars() map[string][]byte {
	t0 := time.Unix(1600000000, 0)
	tn := time.Unix(1600000000, 600000000)
	return map[string][]byte{
		"empty": build(nil),
		"basic": build([]entry{
			{hdr: tar.Header{Typeflag: tar.TypeDir, Name: "a/", Mode: 0o755, ModTime: t0}},
			{hdr: tar.Header{Typeflag: tar.TypeReg, Name: "a/small", Mode: 0o644, Uname: "root", Gname: "root", ModTime: t0}, data: content(1, 100)},
			{hdr: tar.Header{Typeflag: tar.TypeReg, Name: "a/empty", Mode: 0o600, Uname: "root", Gname: "root", ModTime: t0}},
			{hdr: tar.Header{Typeflag: tar.TypeSymlink, Name: "a/link", Linkname: "small", Mode: 0o777, ModTime: t0}},
			{hdr: tar.Header{Typeflag: tar.TypeLink, Name: "a/hard", Linkname: "a/small", ModTime: t0}},
			{hdr: tar.Header{Typeflag: tar.TypeReg, Name: "a/x", Mode: 0o644, Uid: 1000, Gid: 1000, Uname: "u", Gname: "g", ModTime: tn,
				PAXRecords: map[string]string{"SCHILY.xattr.user.k": "v", "SCHILY.xattr.security.capability": "\x01\x02"}}, data: content(2, 5000)},
			{hdr: tar.Header{Typeflag: tar.TypeReg, Name: "a/y", Mode: 0o4755, Uid: 1000, Gid: 1000, Uname: "u", Gname: "g", ModTime: t0}, data: content(3, 513)},
			{hdr: tar.Header{Typeflag: tar.TypeReg, Name: "a/" + string(bytes.Repeat([]byte("long-name-"), 15)), Mode: 0o644, ModTime: t0}, data: content(4, 1)},
			{hdr: tar.Header{Typeflag: tar.TypeFifo, Name: "a/fifo", Mode: 0o644, ModTime: t0}},
			{hdr: tar.Header{Typeflag: tar.TypeChar, Name: "a/null", Mode: 0o666, Devmajor: 1, Devminor: 3, ModTime: t0}},
			{hdr: tar.Header{Typeflag: tar.TypeReg, Name: "a/ünïcode", Mode: 0o644, Uid: 1, Uname: "daemon", ModTime: time.Unix(0, 0)}, data: content(5, 7)},
		}),
		"big": build([]entry{
			{hdr: tar.Header{Typeflag: tar.TypeDir, Name: "big/", Mode: 0o755, ModTime: t0}},
			{hdr: tar.Header{Typeflag: tar.TypeReg, Name: "big/file", Mode: 0o644, ModTime: t0}, data: content(6, 9<<20+123)},
			{hdr: tar.Header{Typeflag: tar.TypeReg, Name: "big/after", Mode: 0o644, ModTime: t0}, data: content(7, 4<<20)},
		}),
	}
}

func sum(b []byte) string {
	s := sha256.Sum256(b)
	return "sha256:" + hex.EncodeToString(s[:])
}

func main() {
	var out []map[string]any
	all := tars()
	for _, name := range []string{"empty", "basic", "big"} {
		tarBytes := all[name]
		for _, level := range []int{gzip.DefaultCompression, gzip.BestCompression, gzip.BestSpeed, gzip.NoCompression} {
			var blob bytes.Buffer
			w := estargz.NewWriterLevel(&blob, level)
			if err := w.AppendTarLossLess(bytes.NewReader(tarBytes)); err != nil {
				panic(err)
			}
			toc, err := w.Close()
			if err != nil {
				panic(err)
			}
			// The DiffID's tar, as the blob holds it.
			zr, err := gzip.NewReader(bytes.NewReader(blob.Bytes()))
			if err != nil {
				panic(err)
			}
			n, err := io.Copy(io.Discard, zr)
			if err != nil {
				panic(err)
			}
			c := map[string]any{
				"tar":          name,
				"tar_digest":   sum(tarBytes),
				"level":        level,
				"blob_len":     blob.Len(),
				"blob_digest":  sum(blob.Bytes()),
				"toc_digest":   toc.String(),
				"diff_id":      w.DiffID(),
				"uncompressed": n,
			}
			if blob.Len() < 4096 {
				c["blob_hex"] = hex.EncodeToString(blob.Bytes())
			}
			out = append(out, c)
		}
	}
	b, err := json.MarshalIndent(out, "", " ")
	if err != nil {
		panic(err)
	}
	if err := os.WriteFile(os.Args[1], append(b, '\n'), 0o644); err != nil {
		panic(err)
	}
}
