// Package media holds the resolved-video model and its safe DTOs. The
// internal ResolvedVideo carries a signed media URL and must never be
// marshalled directly into command output.
package media

import (
	"crypto/sha256"
	"encoding/hex"
	"strings"
	"unicode"
	"unicode/utf8"
)

// ResolvedVideo is the result of the two-step parse chain. MediaURL is
// signed and therefore excluded from any JSON serialisation.
type ResolvedVideo struct {
	ShareURL    string `json:"-"` // normalized share link (internal)
	LocalID     string `json:"-"` // 12-hex SHA-256 prefix of the share link
	Title       string `json:"-"`
	Author      string `json:"-"`
	MediaURL    string `json:"-"` // signed URL — never serialize
	MediaSource string `json:"-"` // h264VideoInfo | h265VideoInfo | videoUrl
	CodecHint   string `json:"-"` // h264 | h265 | ""
}

// InspectResult is the safe DTO printed by `inspect --json`.
type InspectResult struct {
	LocalID     string `json:"local_id"`
	Title       string `json:"title"`
	Author      string `json:"author"`
	MediaSource string `json:"media_source"`
	CodecHint   string `json:"codec_hint"`
}

// Inspect builds the safe DTO.
func (v ResolvedVideo) Inspect() InspectResult {
	return InspectResult{
		LocalID:     v.LocalID,
		Title:       SanitizeText(v.Title),
		Author:      SanitizeText(v.Author),
		MediaSource: v.MediaSource,
		CodecHint:   v.CodecHint,
	}
}

// LocalID computes the stable local identifier: the first 12 hex characters of
// SHA-256 over the normalized share URL bytes. It is NOT a Tencent
// video id.
func LocalID(normalizedShareURL string) string {
	sum := sha256.Sum256([]byte(normalizedShareURL))
	return hex.EncodeToString(sum[:])[:12]
}

// SanitizeText strips terminal control characters from external text before
// it reaches stdout.
func SanitizeText(s string) string {
	var b strings.Builder
	b.Grow(len(s))
	for _, r := range s {
		if r == '\n' || r == '\t' {
			b.WriteRune(' ')
			continue
		}
		if r < 0x20 || r == 0x7f {
			continue
		}
		b.WriteRune(r)
	}
	return b.String()
}

// MaxBasenameBytes caps the generated file basename.
const MaxBasenameBytes = 200

// SafeTitle sanitises an external title for use inside a file name: path
// separators, control characters and : * ? " < > | are removed, whitespace is
// collapsed, trailing spaces/dots stripped, "." and ".." rejected, and the
// result is truncated to MaxBasenameBytes UTF-8 bytes without breaking runes.
func SafeTitle(title string) string {
	var b strings.Builder
	b.Grow(len(title))
	lastSpace := false
	for _, r := range title {
		switch {
		case r == '/' || r == '\\' || r == ':' || r == '*' || r == '?' || r == '"' || r == '<' || r == '>' || r == '|':
			continue
		case unicode.IsSpace(r):
			// includes \t, \n, \r — collapse any whitespace run to one space
			if !lastSpace {
				b.WriteRune(' ')
				lastSpace = true
			}
			continue
		case r < 0x20 || r == 0x7f:
			continue
		default:
			b.WriteRune(r)
			lastSpace = false
		}
	}
	out := strings.TrimRight(b.String(), " .")
	if out == "." || out == ".." || out == "" {
		return "video"
	}
	for len(out) > MaxBasenameBytes {
		r, size := utf8.DecodeLastRuneInString(out)
		if r == utf8.RuneError && size <= 1 {
			out = out[:len(out)-1]
			continue
		}
		out = out[:len(out)-size]
	}
	out = strings.TrimRight(out, " .")
	if out == "" {
		return "video"
	}
	return out
}
