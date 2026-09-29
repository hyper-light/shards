// Prints what Go's archive/tar, the reader Docker, containerd and BuildKit use, reads from
// each archive named on the command line. src/tar.rs's tests check our reader against it.
//
//	GOTOOLCHAIN=go1.27.1 go run expect.go *.tar > expect.txt
package main

import (
	"archive/tar"
	"fmt"
	"hash/fnv"
	"io"
	"os"
	"path/filepath"
	"sort"
	"strings"
)

func main() {
	for _, path := range os.Args[1:] {
		f, err := os.Open(path)
		if err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		fmt.Printf("archive %s\n", filepath.Base(path))
		tr := tar.NewReader(f)
		for {
			h, err := tr.Next()
			if err == io.EOF {
				fmt.Println("end")
				break
			}
			if err != nil {
				fmt.Printf("error %v\n", err)
				break
			}
			sum := fnv.New64a()
			n, err := io.Copy(sum, tr)
			if err != nil {
				fmt.Printf("error %v\n", err)
				break
			}
			var xattrs []string
			sparse := 0
			for k, v := range h.PAXRecords {
				if name, ok := strings.CutPrefix(k, "SCHILY.xattr."); ok {
					xattrs = append(xattrs, fmt.Sprintf("%x:%x", name, v))
				}
				if strings.HasPrefix(k, "GNU.sparse.") {
					sparse = 1
				}
			}
			sort.Strings(xattrs)
			fmt.Printf("entry flag=%d name=%x link=%x mode=%d uid=%d gid=%d mtime=%d nsec=%d size=%d devmajor=%d devminor=%d sparse=%d xattrs=%s data=%d:%016x\n",
				h.Typeflag, h.Name, h.Linkname, h.Mode, h.Uid, h.Gid, h.ModTime.Unix(), h.ModTime.Nanosecond(),
				h.Size, h.Devmajor, h.Devminor, sparse, strings.Join(xattrs, ","), n, sum.Sum64())
		}
		f.Close()
	}
}
