package apperr

import (
	"context"
	"errors"
	"testing"
)

func TestExitCodesMatchSpecSection9(t *testing.T) {
	cases := map[Code]int{
		InternalError: 1, InvalidArgument: 2,
		AuthRequired: 3, InvalidCredentials: 3,
		NetworkError: 4, Timeout: 4,
		UpstreamError: 5, AccessDenied: 5, RateLimited: 5,
		VideoUnavailable: 6, NoMedia: 6,
		UnsupportedMedia: 7,
		DownloadFailed:   8, DownloadTooLarge: 8,
		FileExists: 9, IOError: 9,
		VerifyFailed:           10,
		SchemaChanged:          11,
		LoginDependencyMissing: 12, LoginBrowserFailed: 12,
		InteractiveRequired: 13,
		LoginCaptureFailed:  14, AuthBusy: 14,
		Cancelled: 130,
	}
	for code, want := range cases {
		if got := ExitCode(New(code, StageArguments, "x")); got != want {
			t.Errorf("code %s: exit %d, want %d", code, got, want)
		}
	}
	if got := ExitCode(nil); got != 0 {
		t.Errorf("nil error exit %d, want 0", got)
	}
}

func TestFromClassifiesContextErrors(t *testing.T) {
	if got := From(context.Canceled); got.Code != Cancelled {
		t.Errorf("canceled → %s", got.Code)
	}
	if got := From(context.DeadlineExceeded); got.Code != Timeout {
		t.Errorf("deadline → %s", got.Code)
	}
	if got := From(errors.New("boom")); got.Code != InternalError {
		t.Errorf("unknown → %s", got.Code)
	}
	inner := New(Timeout, StageDownload, "inner")
	if got := From(inner); got != inner {
		t.Errorf("apperr should pass through unchanged")
	}
}

func TestErrorStringNeverIncludesWrappedErr(t *testing.T) {
	secret := "DO_NOT_LEAK_COOKIE_123"
	e := Wrap(errors.New(secret), NetworkError, StageDownload, "网络失败")
	if s := e.Error(); contains(s, secret) {
		t.Errorf("Error() leaked wrapped secret: %s", s)
	}
}

func contains(s, sub string) bool {
	return len(s) >= len(sub) && (s == sub || len(s) > 0 && (func() bool {
		for i := 0; i+len(sub) <= len(s); i++ {
			if s[i:i+len(sub)] == sub {
				return true
			}
		}
		return false
	})())
}
