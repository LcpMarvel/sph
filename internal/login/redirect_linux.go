//go:build linux

package login

import "syscall"

// redirectStdoutToStderr points fd 1 at fd 2 and returns a restore func.
// Linux has Dup3 instead of Dup2. Best effort: when unavailable the caller
// simply proceeds unmuted.
func redirectStdoutToStderr() (func(), error) {
	saved, err := syscall.Dup(syscall.Stdout)
	if err != nil {
		return nil, err
	}
	if err := syscall.Dup3(syscall.Stderr, syscall.Stdout, 0); err != nil {
		syscall.Close(saved)
		return nil, err
	}
	return func() {
		syscall.Dup3(saved, syscall.Stdout, 0)
		syscall.Close(saved)
	}, nil
}
