package media

import (
	"strings"
	"testing"
)

func TestLocalIDStableAnd12Hex(t *testing.T) {
	a := LocalID("https://weixin.qq.com/sph/AogyNMyA7L")
	b := LocalID("https://weixin.qq.com/sph/AogyNMyA7L")
	c := LocalID("https://weixin.qq.com/sph/Other1234")
	if a != b || a == c {
		t.Errorf("local id stability broken")
	}
	if len(a) != 12 || strings.ContainsAny(a, "ghijklmnopqrstuvwxyz") {
		t.Errorf("local id shape wrong: %q", a)
	}
}

func TestSafeTitle(t *testing.T) {
	cases := []struct{ in, want string }{
		{"正常标题", "正常标题"},
		{"a/b\\c:d*e?f\"g<h>i|j", "abcdefghij"},
		{"控制\u0000字符\u007f过滤", "控制字符过滤"},
		{"多   个 空白\t折叠", "多 个 空白 折叠"},
		{"结尾空格和点... ", "结尾空格和点"},
		{".", "video"},
		{"..", "video"},
		{"", "video"},
		{"   ", "video"},
	}
	for _, c := range cases {
		if got := SafeTitle(c.in); got != c.want {
			t.Errorf("SafeTitle(%q) = %q, want %q", c.in, got, c.want)
		}
	}
}

func TestSafeTitleByteCap(t *testing.T) {
	long := strings.Repeat("字", 150) // 450 bytes
	got := SafeTitle(long)
	if len(got) > MaxBasenameBytes {
		t.Errorf("cap exceeded: %d bytes", len(got))
	}
	// truncation must not break UTF-8: re-encoding round trip
	for _, r := range got {
		if r == 0xFFFD {
			t.Fatalf("broken rune in %q", got)
		}
	}
}

func TestSanitizeTextStripsControlChars(t *testing.T) {
	in := "标题\x1b[31m红\u0007\u007f音\n换行\t制表"
	out := SanitizeText(in)
	if strings.ContainsAny(out, "\x1b\u0007\u007f\n") {
		t.Errorf("control chars survived: %q", out)
	}
	if !strings.Contains(out, "标题") || !strings.Contains(out, "制表") {
		t.Errorf("legitimate text damaged: %q", out)
	}
}

func TestInspectDTODoesNotExposeInternals(t *testing.T) {
	v := ResolvedVideo{
		ShareURL: "https://weixin.qq.com/sph/X",
		MediaURL: "https://cdn.example/v.mp4?sig=SECRET",
	}
	dto := v.Inspect()
	// DTO type has no field for these — compile-time guarantee; runtime check
	// is that Inspect output never includes them.
	if strings.Contains(dto.Title, "SECRET") || strings.Contains(dto.Author, "SECRET") {
		t.Errorf("unexpected leak")
	}
}
