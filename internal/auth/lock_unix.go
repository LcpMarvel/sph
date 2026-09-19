//go:build !windows

package auth

import (
	"errors"
	"os"
	"syscall"
)

// lockHandle takes a non-blocking exclusive flock; errLockBusy means another
// process holds it.
func lockHandle(f *os.File) error {
	if err := syscall.Flock(int(f.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		if errors.Is(err, syscall.EWOULDBLOCK) || errors.Is(err, syscall.EAGAIN) {
			return errLockBusy
		}
		return err
	}
	return nil
}

func unlockHandle(f *os.File) error {
	return syscall.Flock(int(f.Fd()), syscall.LOCK_UN)
}
