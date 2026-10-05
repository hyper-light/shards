//go:build cgo && seccomp

// Added by shards' scripts/seccomp/generate to a checkout of runc v1.3.4: PatchAndLoad
// up to the load, the program and the flags it would load handed back instead.

package patchbpf

import (
	libseccomp "github.com/seccomp/libseccomp-golang"
	"golang.org/x/sys/unix"

	"github.com/opencontainers/runc/libcontainer/configs"
)

// Export is PatchAndLoad without the load.
func Export(config *configs.Seccomp, filter *libseccomp.ScmpFilter) ([]unix.SockFilter, uint, bool, error) {
	fprog, err := enosysPatchFilter(config, filter)
	if err != nil {
		return nil, 0, false, err
	}
	flags, noNewPrivs, err := filterFlags(config, filter)
	if err != nil {
		return nil, 0, false, err
	}
	return fprog, flags, noNewPrivs, nil
}
