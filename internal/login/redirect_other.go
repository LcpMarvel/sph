//go:build !linux && !darwin

package login

import "errors"

// redirectStdoutToStderr is a no-op where fd swapping is not implemented
// (Windows and others); the browser downloader's progress may share stdout
// with the final result during first login.
func redirectStdoutToStderr() (func(), error) {
	return nil, errors.New("stdout redirect not supported on this platform")
}
