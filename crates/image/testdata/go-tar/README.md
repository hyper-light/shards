# Go's tar test archives

`*.tar` are Go's `src/archive/tar/testdata` archives at go1.27.1, unmodified.
`neg-size.tar` is decoded from Go's `neg-size.tar.base64`. They are licensed under Go's
BSD-style licence, in `LICENSE`.

`expect.txt` is what Go's `archive/tar` reads from each archive. Docker, containerd and
BuildKit read and write layers with that package. `src/tar.rs`'s tests hold our reader to
this file. Regenerate it with:

```sh
GOTOOLCHAIN=go1.27.1 go run expect.go *.tar > expect.txt
```
