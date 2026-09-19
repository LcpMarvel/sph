// Package netpolicy enforces the network destination rules: strict
// share-URL validation, media-URL validation, a dialer that refuses
// non-public addresses, and redirect policies.
package netpolicy

import (
	"net"
	"net/url"
	"strings"
	"unicode"

	"sph/internal/apperr"
)

// MaxShareURLBytes is the maximum accepted length of a share link.
const MaxShareURLBytes = 8192

const (
	shareHost       = "weixin.qq.com"
	sharePathPrefix = "/sph/"
)

// NormalizeShareURL validates a 视频号 share link per and returns its
// normalized form: fragment dropped, query kept as-is. The normalized form is
// the input for the local_id hash.
func NormalizeShareURL(raw string) (string, error) {
	trimmed := strings.TrimSpace(raw)
	if trimmed == "" {
		return "", apperr.New(apperr.InvalidArgument, apperr.StageArguments, "未提供分享链接")
	}
	if len(trimmed) > MaxShareURLBytes {
		return "", apperr.Newf(apperr.InvalidArgument, apperr.StageArguments, "链接超过 %d 字节上限", MaxShareURLBytes)
	}
	for _, r := range trimmed {
		if r == '\n' || r == '\r' || r == '\t' || unicode.IsControl(r) {
			return "", apperr.New(apperr.InvalidArgument, apperr.StageArguments, "链接包含换行或控制字符")
		}
	}
	u, err := url.Parse(trimmed)
	if err != nil {
		return "", apperr.New(apperr.InvalidArgument, apperr.StageArguments, "链接格式不合法")
	}
	if u.Scheme != "https" {
		return "", apperr.New(apperr.InvalidArgument, apperr.StageArguments, "链接必须是 HTTPS 协议")
	}
	if u.Hostname() != shareHost {
		return "", apperr.Newf(apperr.InvalidArgument, apperr.StageArguments, "链接主机必须是 %s", shareHost)
	}
	if u.User != nil {
		return "", apperr.New(apperr.InvalidArgument, apperr.StageArguments, "链接不允许携带 userinfo")
	}
	if u.Port() != "" {
		return "", apperr.New(apperr.InvalidArgument, apperr.StageArguments, "链接不允许自定义端口")
	}
	code := strings.TrimPrefix(u.EscapedPath(), sharePathPrefix)
	if code == u.EscapedPath() || code == "" || strings.Contains(code, "/") {
		return "", apperr.New(apperr.InvalidArgument, apperr.StageArguments, "链接路径必须是 /sph/<短码>")
	}
	for _, r := range code {
		if !isShareCodeRune(r) {
			return "", apperr.New(apperr.InvalidArgument, apperr.StageArguments, "短码包含非法字符")
		}
	}
	u.Fragment = ""
	u.RawFragment = ""
	return u.String(), nil
}

func isShareCodeRune(r rune) bool {
	return (r >= 'a' && r <= 'z') || (r >= 'A' && r <= 'Z') || (r >= '0' && r <= '9') || r == '_' || r == '-'
}

// ValidateMediaURL checks a media URL returned by the upstream API per 8.1: HTTPS, no userinfo, no custom port, and not an IP literal. The raw
// string itself must be used verbatim for the request; this check
// never rewrites it.
func ValidateMediaURL(raw string) error {
	u, err := url.Parse(raw)
	if err != nil {
		return apperr.New(apperr.SchemaChanged, apperr.StageSelectMedia, "媒体地址不合法")
	}
	if u.Scheme != "https" {
		return apperr.New(apperr.SchemaChanged, apperr.StageSelectMedia, "媒体地址必须是 HTTPS")
	}
	if u.Hostname() == "" {
		return apperr.New(apperr.SchemaChanged, apperr.StageSelectMedia, "媒体地址缺少主机名")
	}
	if u.User != nil {
		return apperr.New(apperr.SchemaChanged, apperr.StageSelectMedia, "媒体地址不允许携带 userinfo")
	}
	if u.Port() != "" {
		return apperr.New(apperr.SchemaChanged, apperr.StageSelectMedia, "媒体地址不允许自定义端口")
	}
	if ip := net.ParseIP(u.Hostname()); ip != nil {
		return apperr.New(apperr.SchemaChanged, apperr.StageSelectMedia, "媒体地址不允许是 IP 字面量")
	}
	return nil
}

// ValidatePreviewURL checks a playable_url per
func ValidatePreviewURL(raw string) (*url.URL, error) {
	u, err := url.Parse(raw)
	if err != nil || u.Scheme != "https" || u.Hostname() != "channels.weixin.qq.com" {
		return nil, apperr.New(apperr.SchemaChanged, apperr.StageParseShare, "playable_url 不是预期的视频号预览地址")
	}
	if u.User != nil || u.Port() != "" {
		return nil, apperr.New(apperr.SchemaChanged, apperr.StageParseShare, "playable_url 不允许携带 userinfo 或自定义端口")
	}
	if u.EscapedPath() != "/finder-preview/pages/feed" {
		return nil, apperr.New(apperr.SchemaChanged, apperr.StageParseShare, "playable_url 路径结构已变化")
	}
	return u, nil
}

// CountQueryKeys reports how many times each key occurs in a raw query string.
func CountQueryKeys(rawQuery string) map[string]int {
	counts := make(map[string]int)
	for rawQuery != "" {
		var pair string
		if i := strings.IndexByte(rawQuery, '&'); i >= 0 {
			pair, rawQuery = rawQuery[:i], rawQuery[i+1:]
		} else {
			pair, rawQuery = rawQuery, ""
		}
		if pair == "" {
			continue
		}
		key := pair
		if i := strings.IndexByte(pair, '='); i >= 0 {
			key = pair[:i]
		}
		if decoded, err := url.QueryUnescape(key); err == nil {
			key = decoded
		}
		counts[key]++
	}
	return counts
}
