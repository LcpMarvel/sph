// Package apperr defines the stable error model for sph: machine-readable
// error codes, a processing stage, safe human messages and stable process
// exit codes.
//
// Message MUST stay safe to print: no cookies, session headers, generalToken,
// signed media URLs or raw upstream bodies. The wrapped Err is never rendered.
package apperr

import (
	"context"
	"errors"
	"fmt"
)

// Code is a stable machine-readable error identifier.
type Code string

const (
	InternalError          Code = "INTERNAL_ERROR"
	InvalidArgument        Code = "INVALID_ARGUMENT"
	AuthRequired           Code = "AUTH_REQUIRED"
	InvalidCredentials     Code = "INVALID_CREDENTIALS"
	NetworkError           Code = "NETWORK_ERROR"
	Timeout                Code = "TIMEOUT"
	UpstreamError          Code = "UPSTREAM_ERROR"
	AccessDenied           Code = "ACCESS_DENIED"
	RateLimited            Code = "RATE_LIMITED"
	VideoUnavailable       Code = "VIDEO_UNAVAILABLE"
	NoMedia                Code = "NO_MEDIA"
	UnsupportedMedia       Code = "UNSUPPORTED_MEDIA"
	DownloadFailed         Code = "DOWNLOAD_FAILED"
	DownloadTooLarge       Code = "DOWNLOAD_TOO_LARGE"
	FileExists             Code = "FILE_EXISTS"
	IOError                Code = "IO_ERROR"
	VerifyFailed           Code = "VERIFY_FAILED"
	SchemaChanged          Code = "SCHEMA_CHANGED"
	LoginDependencyMissing Code = "LOGIN_DEPENDENCY_MISSING"
	LoginBrowserFailed     Code = "LOGIN_BROWSER_FAILED"
	InteractiveRequired    Code = "INTERACTIVE_REQUIRED"
	LoginCaptureFailed     Code = "LOGIN_CAPTURE_FAILED"
	AuthBusy               Code = "AUTH_BUSY"
	Cancelled              Code = "CANCELLED"
)

// Stage identifies where in the pipeline an error occurred.
type Stage string

const (
	StageArguments    Stage = "arguments"
	StageCredentials  Stage = "credentials"
	StageLoginDeps    Stage = "login_dependencies"
	StageLoginBrowser Stage = "login_browser"
	StageLoginCapture Stage = "login_capture"
	StageLoginCommit  Stage = "login_commit"
	StageParseShare   Stage = "parse_share"
	StageFetchFeed    Stage = "fetch_feed"
	StageSelectMedia  Stage = "select_media"
	StageDownload     Stage = "download"
	StageVerify       Stage = "verify"
	StageCommit       Stage = "commit"
)

// exitCodes maps every code to its process exit code.
var exitCodes = map[Code]int{
	InternalError:          1,
	InvalidArgument:        2,
	AuthRequired:           3,
	InvalidCredentials:     3,
	NetworkError:           4,
	Timeout:                4,
	UpstreamError:          5,
	AccessDenied:           5,
	RateLimited:            5,
	VideoUnavailable:       6,
	NoMedia:                6,
	UnsupportedMedia:       7,
	DownloadFailed:         8,
	DownloadTooLarge:       8,
	FileExists:             9,
	IOError:                9,
	VerifyFailed:           10,
	SchemaChanged:          11,
	LoginDependencyMissing: 12,
	LoginBrowserFailed:     12,
	InteractiveRequired:    13,
	LoginCaptureFailed:     14,
	AuthBusy:               14,
	Cancelled:              130,
}

// Error is the error type carried between internal packages.
type Error struct {
	Code      Code
	Stage     Stage
	Message   string
	Retryable bool
	Err       error // underlying cause; never printed
}

func (e *Error) Error() string {
	return fmt.Sprintf("%s (%s): %s", e.Code, e.Stage, e.Message)
}

func (e *Error) Unwrap() error { return e.Err }

// New builds an *Error without an underlying cause.
func New(code Code, stage Stage, msg string) *Error {
	return &Error{Code: code, Stage: stage, Message: msg}
}

// Newf is New with formatting.
func Newf(code Code, stage Stage, format string, args ...any) *Error {
	return &Error{Code: code, Stage: stage, Message: fmt.Sprintf(format, args...)}
}

// Wrap attaches an underlying cause that is kept for diagnostics but never
// rendered to the user.
func Wrap(err error, code Code, stage Stage, msg string) *Error {
	return &Error{Code: code, Stage: stage, Message: msg, Err: err}
}

// From coerces any error into an *Error. Known context errors are classified;
// anything else becomes INTERNAL_ERROR unless it already is an *Error.
func From(err error) *Error {
	if err == nil {
		return nil
	}
	var ae *Error
	if errors.As(err, &ae) {
		return ae
	}
	switch {
	case errors.Is(err, context.Canceled):
		return Wrap(err, Cancelled, StageArguments, "操作已取消")
	case errors.Is(err, context.DeadlineExceeded):
		return Wrap(err, Timeout, StageArguments, "操作超时")
	}
	return Wrap(err, InternalError, StageArguments, "未预期的内部错误")
}

// ExitCode maps err to the stable process exit code; nil maps to 0.
func ExitCode(err error) int {
	if err == nil {
		return 0
	}
	if code, ok := exitCodes[From(err).Code]; ok {
		return code
	}
	return 1
}

// CodeOf returns the apperr code of err, or "" when err is nil.
func CodeOf(err error) Code {
	if err == nil {
		return ""
	}
	return From(err).Code
}

// StageOf returns the stage of err, or "" when err is nil.
func StageOf(err error) Stage {
	if err == nil {
		return ""
	}
	return From(err).Stage
}
