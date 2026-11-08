//go:build cgo && seccomp

// Added by shards' scripts/seccomp/generate to a checkout of runc v1.3.4: InitSeccomp up
// to the load, the libseccomp filter built as it builds it, handed back instead.

package seccomp

import (
	"errors"
	"fmt"

	libseccomp "github.com/seccomp/libseccomp-golang"

	"github.com/opencontainers/runc/libcontainer/configs"
)

// BuildFilter is InitSeccomp without patchbpf.PatchAndLoad.
func BuildFilter(config *configs.Seccomp) (*libseccomp.ScmpFilter, error) {
	if config == nil {
		return nil, errors.New("cannot initialize Seccomp - nil config passed")
	}
	defaultAction, err := getAction(config.DefaultAction, config.DefaultErrnoRet)
	if err != nil {
		return nil, errors.New("error initializing seccomp - invalid default action")
	}
	if defaultAction == libseccomp.ActNotify {
		return nil, errors.New("SCMP_ACT_NOTIFY cannot be used as default action")
	}
	for _, call := range config.Syscalls {
		if call.Action == configs.Notify && call.Name == "write" {
			return nil, errors.New("SCMP_ACT_NOTIFY cannot be used for the write syscall")
		}
	}
	filter, err := libseccomp.NewFilter(defaultAction)
	if err != nil {
		return nil, fmt.Errorf("error creating filter: %w", err)
	}
	for _, arch := range config.Architectures {
		scmpArch, err := libseccomp.GetArchFromString(arch)
		if err != nil {
			return nil, fmt.Errorf("error validating Seccomp architecture: %w", err)
		}
		if err := filter.AddArch(scmpArch); err != nil {
			return nil, fmt.Errorf("error adding architecture to seccomp filter: %w", err)
		}
	}
	for _, flag := range config.Flags {
		if err := setFlag(filter, flag); err != nil {
			return nil, err
		}
	}
	if len(config.Syscalls) > 32 {
		_ = filter.SetOptimize(2)
	}
	if err := filter.SetNoNewPrivsBit(false); err != nil {
		return nil, fmt.Errorf("error setting no new privileges: %w", err)
	}
	for _, call := range config.Syscalls {
		if call == nil {
			return nil, errors.New("encountered nil syscall while initializing Seccomp")
		}
		if err := matchCall(filter, call, defaultAction); err != nil {
			return nil, err
		}
	}
	return filter, nil
}
