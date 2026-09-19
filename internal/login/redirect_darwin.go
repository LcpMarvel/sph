//go:build darwin

package login

import "syscall"

// redirectStdoutToStderr points fd 1 at fd 2 and returns a restore func.
// Best effort: when unavailable the caller simply proceeds unmuted.
func redirectStdoutToStderr() (func(), error) {
	saved, err := syscall.Dup(syscall.Stdout)
	if err != nil {
		return nil, err
	}
	if err := syscall.Dup2(syscall.Stderr, syscall.Stdout); err != nil {
		syscall.Close(saved)
		return nil, err
	}
	return func() {
		syscall.Dup2(saved, syscall.Stdout)
		syscall.Close(saved)
	}, nil
}
