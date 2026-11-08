// Prints, for each archive, when the newest member Go's archive/tar calls regular was
// modified, as BuildKit takes a source's time from an archive (dockerfile/1.27.1
// frontend/dockerfile/dockerfile2llb/epoch.go, archiveMaxTimeFromRef, with
// allowNonArchive): "newest SECS NSEC", "none", or "error ERR" where Go fails, which
// BuildKit reads as none. src/tar.rs's tests check Reader::newest_regular against it.
//
// It first writes this directory's archives, what Go's own test archives (../go-tar)
// lack, with headers written byte by byte, as Go's writer would not write some of them.
//
//	GOTOOLCHAIN=go1.27.1 go run newest.go ../go-tar/*.tar > newest.txt
package main

import (
	"archive/tar"
	"bytes"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"time"
)

type member struct {
	name  string
	flag  byte
	mode  int64
	mtime int64
	data  string
	link  string
}

func octal(b []byte, v int64) {
	s := fmt.Sprintf("%0*o", len(b)-1, v)
	copy(b, s[len(s)-(len(b)-1):])
	b[len(b)-1] = 0
}

func header(m member) []byte {
	b := make([]byte, 512)
	copy(b[0:100], m.name)
	octal(b[100:108], m.mode)
	octal(b[108:116], 0)
	octal(b[116:124], 0)
	octal(b[124:136], int64(len(m.data)))
	octal(b[136:148], m.mtime)
	b[156] = m.flag
	copy(b[157:257], m.link)
	copy(b[257:263], "ustar\x00")
	copy(b[263:265], "00")
	for i := 148; i < 156; i++ {
		b[i] = ' '
	}
	sum := 0
	for _, c := range b {
		sum += int(c)
	}
	octal(b[148:155], int64(sum))
	return b
}

func archive(members ...member) []byte {
	var out bytes.Buffer
	for _, m := range members {
		out.Write(header(m))
		out.WriteString(m.data)
		if pad := len(m.data) % 512; pad != 0 {
			out.Write(make([]byte, 512-pad))
		}
	}
	out.Write(make([]byte, 1024))
	return out.Bytes()
}

// A PAX record: its length counts itself.
func record(k, v string) string {
	payload := " " + k + "=" + v + "\n"
	n := len(payload) + 1
	for len(fmt.Sprint(n))+len(payload) != n {
		n++
	}
	return fmt.Sprint(n) + payload
}

func file(name string, mtime int64) member {
	return member{name: name, flag: '0', mode: 0o644, mtime: mtime}
}

var crafted = map[string][]byte{
	"global-only.tar": archive(member{name: "pax_global_header", flag: 'g', mode: 0o666, mtime: 1800000000, data: record("comment", "abc")}),
	"global-then-file.tar": archive(
		member{name: "pax_global_header", flag: 'g', mode: 0o666, mtime: 1800000000, data: record("comment", "abc")},
		file("a", 1700000000)),
	"dir-mode-bits.tar": archive(member{name: "d", flag: '0', mode: 0o40755, mtime: 1800000000}, file("a", 1700000000)),
	"fifo-mode-bits.tar": archive(member{name: "p", flag: '0', mode: 0o10644, mtime: 1800000000}, file("a", 1700000000)),
	"rega-slash.tar": archive(member{name: "d/", flag: 0, mode: 0o755, mtime: 1800000000}, member{name: "a", flag: 0, mode: 0o644, mtime: 1700000000}),
	"unknown-type.tar": archive(member{name: "z", flag: 'Z', mode: 0o644, mtime: 1800000000, data: "abc"}, member{name: "d", flag: '5', mode: 0o755, mtime: 1900000000}),
	"pax-nsec.tar": archive(
		member{name: "PaxHeader", flag: 'x', mode: 0o644, data: record("mtime", "1700000000.5")},
		file("a", 1),
		file("b", 1700000000)),
	"links.tar": archive(
		file("a", 1700000000),
		member{name: "b", flag: '1', mode: 0o644, mtime: 1800000000, link: "a"},
		member{name: "c", flag: '2', mode: 0o777, mtime: 1900000000, link: "a"}),
	"pax-path-slash.tar": archive(
		member{name: "PaxHeader", flag: 'x', mode: 0o644, data: record("path", "d/")},
		member{name: "x", flag: 0, mode: 0o755, mtime: 1800000000},
		file("a", 1700000000)),
	"long-name-slash.tar": archive(
		member{name: "././@LongLink", flag: 'L', mode: 0o644, data: "e/\x00"},
		member{name: "y", flag: 0, mode: 0o755, mtime: 1800000000},
		file("a", 1700000000)),
	"global-drops-pax.tar": archive(
		member{name: "PaxHeader", flag: 'x', mode: 0o644, data: record("mtime", "1900000000")},
		member{name: "pax_global_header", flag: 'g', mode: 0o666, data: record("comment", "abc")},
		file("a", 1700000000)),
	"bad-pax-uid.tar": archive(
		member{name: "PaxHeader", flag: 'x', mode: 0o644, data: record("uid", "abc")},
		file("a", 1700000000)),
	"bad-global-uid.tar": archive(
		member{name: "pax_global_header", flag: 'g', mode: 0o666, data: record("uid", "abc")},
		file("a", 1700000000)),
	"empty.tar": archive(),
	"garbage.tar": bytes.Repeat([]byte("x"), 600),
}

func newest(r io.Reader) (*time.Time, error) {
	tr := tar.NewReader(r)
	var max *time.Time
	for {
		hdr, err := tr.Next()
		if err != nil {
			if errors.Is(err, io.EOF) {
				return max, nil
			}
			return nil, err
		}
		if !hdr.FileInfo().Mode().IsRegular() {
			continue
		}
		tm := hdr.ModTime.UTC()
		if max == nil || tm.After(*max) {
			max = &tm
		}
	}
}

func main() {
	paths := os.Args[1:]
	names := make([]string, 0, len(crafted))
	for name := range crafted {
		names = append(names, name)
	}
	// In order, as the shell lists the others.
	for i := range names {
		for j := i + 1; j < len(names); j++ {
			if names[j] < names[i] {
				names[i], names[j] = names[j], names[i]
			}
		}
	}
	for _, name := range names {
		if err := os.WriteFile(name, crafted[name], 0o644); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		paths = append(paths, filepath.Join("..", "tar-newest", name))
	}
	for _, path := range paths {
		f, err := os.Open(path)
		if err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		fmt.Printf("archive %s/%s\n", filepath.Base(filepath.Dir(path)), filepath.Base(path))
		tm, err := newest(f)
		switch {
		case err != nil:
			fmt.Printf("error %v\n", err)
		case tm == nil:
			fmt.Println("none")
		default:
			fmt.Printf("newest %d %d\n", tm.Unix(), tm.Nanosecond())
		}
		f.Close()
	}
}
