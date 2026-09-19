//go:build windows

package auth

import (
	"errors"
	"os"
	"syscall"
	"unsafe"
)

const (
	lockfileFailImmediately = 0x00000001
	lockfileExclusiveLock   = 0x00000002
	errorLockViolation      = 33 // ERROR_LOCK_VIOLATION
)

var (
	kernel32         = syscall.NewLazyDLL("kernel32.dll")
	procLockFileEx   = kernel32.NewProc("LockFileEx")
	procUnlockFileEx = kernel32.NewProc("UnlockFileEx")
)

// lockHandle takes a non-blocking exclusive byte-range lock via LockFileEx;
// errLockBusy means another process holds it. The lock is released by
// UnlockFileEx or by closing the handle (process exit included).
func lockHandle(f *os.File) error {
	// BOOL LockFileEx(HANDLE, DWORD flags, DWORD reserved,
	//                 DWORD nBytesLow, DWORD nBytesHigh, LPOVERLAPPED)
	ol := new(syscall.Overlapped)
	r1, _, err := procLockFileEx.Call(f.Fd(),
		lockfileExclusiveLock|lockfileFailImmediately, 0, 1, 0,
		uintptr(unsafe.Pointer(ol)))
	if r1 == 0 {
		if errors.Is(err, syscall.Errno(errorLockViolation)) {
			return errLockBusy
		}
		return err
	}
	return nil
}

func unlockHandle(f *os.File) error {
	// BOOL UnlockFileEx(HANDLE, DWORD reserved,
	//                    DWORD nBytesLow, DWORD nBytesHigh, LPOVERLAPPED)
	ol := new(syscall.Overlapped)
	r1, _, err := procUnlockFileEx.Call(f.Fd(), 0, 1, 0, uintptr(unsafe.Pointer(ol)))
	if r1 == 0 {
		return err
	}
	return nil
}
