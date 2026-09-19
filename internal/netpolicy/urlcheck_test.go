package netpolicy

import (
	"strings"
	"testing"

	"sph/internal/apperr"
)

func TestNormalizeShareURLAccepts(t *testing.T) {
	cases := []struct{ in, want string }{
		{"https://weixin.qq.com/sph/AogyNMyA7L", "https://weixin.qq.com/sph/AogyNMyA7L"},
		{"  https://weixin.qq.com/sph/abc-123_X  ", "https://weixin.qq.com/sph/abc-123_X"},
		{"https://weixin.qq.com/sph/code?ch=a&x=1", "https://weixin.qq.com/sph/code?ch=a&x=1"},
		// fragment dropped
		{"https://weixin.qq.com/sph/code#share", "https://weixin.qq.com/sph/code"},
	}
	for _, c := range cases {
		got, err := NormalizeShareURL(c.in)
		if err != nil {
			t.Errorf("%q: unexpected error %v", c.in, err)
			continue
		}
		if got != c.want {
			t.Errorf("%q: %q, want %q", c.in, got, c.want)
		}
	}
}

func TestNormalizeShareURLRejects(t *testing.T) {
	cases := []string{
		"",
		"   ",
		"http://weixin.qq.com/sph/a",            // not https
		"https://weixin.qq.com.evil.test/sph/a", // subdomain spoof
		"https://evil.test/sph/a",               // other host
		"https://user:pass@weixin.qq.com/sph/a", // userinfo
		"https://weixin.qq.com@evil.test/sph/a", // userinfo spoof
		"https://weixin.qq.com:8443/sph/a",      // custom port
		"https://weixin.qq.com/sph/",            // empty code
		"https://weixin.qq.com/sph/a/b",         // two segments
		"https://weixin.qq.com/other/a",         // wrong prefix
		"https://weixin.qq.com/sph/打",           // non-ascii code
		"https://weixin.qq.com/sph/a%20b",       // encoded space in code
		"https://weixin.qq.com/sph/a\nb",        // newline
		"https://weixin.qq.com/sph/a b",         // inner whitespace
		"javascript:alert(1)",
		"file:///etc/passwd",
		"https://weixin.qq.com/sph/a https://weixin.qq.com/sph/b", // two links
		"https://weixin.qq.com/sph/" + strings.Repeat("a", 8192),  // over the byte cap
		"https://weixin.qq.com/sph/a\x00b",                        // control char
	}
	for _, in := range cases {
		if _, err := NormalizeShareURL(in); err == nil {
			t.Errorf("%q: expected rejection", truncate(in))
		} else if apperr.CodeOf(err) != apperr.InvalidArgument {
			t.Errorf("%q: code %s, want INVALID_ARGUMENT", truncate(in), apperr.CodeOf(err))
		}
	}
}

func TestValidateMediaURL(t *testing.T) {
	good := []string{
		"https://example.com/v.mp4?X-snsvideoflag=1&encfilekey=abc",
		"https://cdn.example.com/a/b/c.mp4",
	}
	for _, u := range good {
		if err := ValidateMediaURL(u); err != nil {
			t.Errorf("%q: unexpected error %v", u, err)
		}
	}
	bad := map[string]string{
		"http://example.com/v.mp4":       "scheme",
		"https://1.2.3.4/v.mp4":          "ip literal",
		"https://user@example.com/v.mp4": "userinfo",
		"https://example.com:8443/v.mp4": "port",
		"/relative":                      "relative",
		"":                               "empty",
		"https://[::1]/v.mp4":            "ipv6 loopback literal",
	}
	for u := range bad {
		if err := ValidateMediaURL(u); err == nil {
			t.Errorf("%q: expected rejection", u)
		}
	}
}

func TestCountQueryKeys(t *testing.T) {
	c := CountQueryKeys("token=a&eid=b&token=c&x=1")
	if c["token"] != 2 || c["eid"] != 1 {
		t.Errorf("counts wrong: %#v", c)
	}
}

func truncate(s string) string {
	if len(s) > 40 {
		return s[:40] + "…"
	}
	return s
}
