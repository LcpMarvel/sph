// Package auth stores the single local credentials file, enforces the extra
// header whitelist, provides the advisory modification lock shared by login /
// import / logout, and parses the manual-import backup path.
package auth

import (
	"encoding/json"
	"errors"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/LcpMarvel/sph-downloader/internal/apperr"
)

// Size limits
const (
	MaxCredentialsBytes = 256 << 10
	MaxCookieBytes      = 64 << 10
	MaxHeadersBytes     = 64 << 10
	MaxImportBytes      = 64 << 10

	credentialsFile    = "credentials.json"
	lockFile           = ".auth.lock"
	CredentialsVersion = 1
)

// Source records how the stored credentials were obtained.
type Source string

const (
	SourceBrowserLogin Source = "browser_login"
	SourceManualImport Source = "manual_import"
)

// Credentials is the in-memory form of credentials.json. Cookie and header
// values are secrets: they must never be logged or marshalled into output.
type Credentials struct {
	Version        int
	SavedAt        time.Time
	Source         Source
	VerifiedAt     *time.Time
	Cookie         string
	YuanbaoHeaders map[string]string // canonical lower-case keys
}

// fileFormat mirrors credentials.json exactly.
type fileFormat struct {
	Version        *int              `json:"version"`
	SavedAt        time.Time         `json:"saved_at"`
	Source         string            `json:"source"`
	VerifiedAt     *time.Time        `json:"verified_at"`
	Cookie         string            `json:"cookie"`
	YuanbaoHeaders map[string]string `json:"yuanbao_headers"`
}

// allowedYuanbaoHeaderKeys is the exact whitelist
var allowedYuanbaoHeaderKeys = map[string]struct{}{
	"accept-language": {}, "user-agent": {}, "referer": {},
	"sec-ch-ua": {}, "sec-ch-ua-mobile": {}, "sec-ch-ua-platform": {},
	"sec-fetch-dest": {}, "sec-fetch-mode": {}, "sec-fetch-site": {},
	"t-userid": {}, "x-agentid": {}, "x-commit-tag": {}, "x-device-id": {},
	"x-hy106": {}, "x-hy92": {}, "x-hy93": {}, "x-id": {}, "x-instance-id": {},
	"x-language": {}, "x-os_version": {}, "x-platform": {}, "x-requested-with": {},
	"x-source": {}, "x-web-third-source": {}, "x-webdriver": {}, "x-webversion": {},
	"x-ybuitest": {},
}

// forbiddenHeaderKeys must never be overridable via headers.
var forbiddenHeaderKeys = map[string]struct{}{
	"cookie": {}, "authorization": {}, "host": {}, "origin": {},
	"content-length": {}, "connection": {}, "transfer-encoding": {},
	"proxy-authorization": {},
}

// CanonicalHeaderKey lower-cases an ASCII header name.
func CanonicalHeaderKey(key string) string {
	return strings.ToLower(strings.TrimSpace(key))
}

// AllowedHeaderKeys returns the whitelist keys (for docs/tests).
func AllowedHeaderKeys() map[string]struct{} { return allowedYuanbaoHeaderKeys }

// ValidateHeaderMap canonicalises and validates an extra-header map per
// : only whitelisted keys, no CR/LF in values, referer restricted to
// the yuanbao site, forbidden keys rejected, total size capped. Unknown keys
// are an error so a bad import or capture fails loudly instead of silently
// dropping data.
func ValidateHeaderMap(in map[string]string) (map[string]string, error) {
	if in == nil {
		return nil, nil
	}
	total := 0
	out := make(map[string]string, len(in))
	for k, v := range in {
		ck := CanonicalHeaderKey(k)
		if _, bad := forbiddenHeaderKeys[ck]; bad {
			return nil, apperr.Newf(apperr.InvalidArgument, apperr.StageCredentials,
				"额外请求头不允许覆盖 %s", ck)
		}
		if _, ok := allowedYuanbaoHeaderKeys[ck]; !ok {
			return nil, apperr.Newf(apperr.InvalidArgument, apperr.StageCredentials,
				"额外请求头 %s 不在允许列表内", ck)
		}
		if strings.ContainsAny(v, "\r\n") {
			return nil, apperr.Newf(apperr.InvalidArgument, apperr.StageCredentials,
				"额外请求头 %s 的值包含换行符", ck)
		}
		if ck == "referer" {
			u, err := url.Parse(v)
			if err != nil || u.Scheme != "https" || u.Hostname() != "yuanbao.tencent.com" {
				return nil, apperr.New(apperr.InvalidArgument, apperr.StageCredentials,
					"referer 只允许 https://yuanbao.tencent.com")
			}
		}
		total += len(ck) + len(v)
		if total > MaxHeadersBytes {
			return nil, apperr.Newf(apperr.InvalidArgument, apperr.StageCredentials,
				"额外请求头总大小超过 %d 字节", MaxHeadersBytes)
		}
		out[ck] = v
	}
	return out, nil
}

// Validate checks a complete Credentials value before it is stored or used.
func (c Credentials) Validate() error {
	if c.Cookie == "" {
		return apperr.New(apperr.InvalidCredentials, apperr.StageCredentials, "凭证缺少 Cookie")
	}
	if len(c.Cookie) > MaxCookieBytes {
		return apperr.Newf(apperr.InvalidCredentials, apperr.StageCredentials, "Cookie 超过 %d 字节上限", MaxCookieBytes)
	}
	if strings.ContainsAny(c.Cookie, "\r\n") {
		return apperr.New(apperr.InvalidCredentials, apperr.StageCredentials, "Cookie 包含换行符")
	}
	if c.Source != SourceBrowserLogin && c.Source != SourceManualImport {
		return apperr.New(apperr.InvalidCredentials, apperr.StageCredentials, "凭证来源字段的值不合法")
	}
	if _, err := ValidateHeaderMap(c.YuanbaoHeaders); err != nil {
		return err
	}
	return nil
}

// ErrNoCredentials marks "no credentials file present".
var ErrNoCredentials = errors.New("未找到本地凭证")

// Store manages the credentials file inside a config directory.
type Store struct {
	Dir string
}

// DefaultConfigDir honours $SPH_CONFIG_DIR, else uses ~/.config/sph.
func DefaultConfigDir() string {
	if dir := os.Getenv("SPH_CONFIG_DIR"); dir != "" {
		return dir
	}
	home, err := os.UserHomeDir()
	if err != nil || home == "" {
		return ".config/sph"
	}
	return filepath.Join(home, ".config", "sph")
}

// NewStore makes sure the config directory exists with 0700 permissions.
func NewStore(dir string) (*Store, error) {
	s := &Store{Dir: dir}
	info, err := os.Lstat(dir)
	if os.IsNotExist(err) {
		if err := os.MkdirAll(dir, 0o700); err != nil {
			return nil, apperr.Wrap(err, apperr.IOError, apperr.StageCredentials, "无法创建配置目录")
		}
		return s, nil
	}
	if err != nil {
		return nil, apperr.Wrap(err, apperr.IOError, apperr.StageCredentials, "无法访问配置目录")
	}
	if !info.IsDir() {
		return nil, apperr.New(apperr.IOError, apperr.StageCredentials, "配置路径不是目录")
	}
	if info.Mode().Perm()&0o077 != 0 {
		if err := os.Chmod(dir, 0o700); err != nil {
			return nil, apperr.Wrap(err, apperr.IOError, apperr.StageCredentials, "配置目录权限过宽且无法收紧")
		}
	}
	return s, nil
}

// Path is the absolute credentials file path.
func (s *Store) Path() string { return filepath.Join(s.Dir, credentialsFile) }

// LockPath is the advisory modification lock path.
func (s *Store) LockPath() string { return filepath.Join(s.Dir, lockFile) }

// Exists reports whether a credentials file is present.
func (s *Store) Exists() bool {
	_, err := os.Lstat(s.Path())
	return err == nil
}

// Load reads and validates credentials. Missing file yields ErrNoCredentials
// wrapped in an AUTH_REQUIRED apperr. Symlinks, wide permissions, oversize,
// unknown versions and invalid content are refused with repair hints.
func (s *Store) Load() (Credentials, error) {
	path := s.Path()
	info, err := os.Lstat(path)
	if os.IsNotExist(err) {
		return Credentials{}, apperr.Wrap(ErrNoCredentials, apperr.AuthRequired, apperr.StageCredentials,
			"未找到登录凭证，请先执行 sph login")
	}
	if err != nil {
		return Credentials{}, apperr.Wrap(err, apperr.IOError, apperr.StageCredentials, "无法读取凭证文件")
	}
	if info.Mode()&os.ModeSymlink != 0 || !info.Mode().IsRegular() {
		return Credentials{}, apperr.Newf(apperr.InvalidCredentials, apperr.StageCredentials,
			"凭证文件不是普通文件，请检查 %s", path)
	}
	if info.Mode().Perm()&0o077 != 0 {
		return Credentials{}, apperr.Newf(apperr.InvalidCredentials, apperr.StageCredentials,
			"凭证文件权限过宽，请执行 chmod 600 %s", path)
	}
	if info.Size() > MaxCredentialsBytes {
		return Credentials{}, apperr.New(apperr.InvalidCredentials, apperr.StageCredentials, "凭证文件超过大小上限")
	}
	raw, err := os.ReadFile(path)
	if err != nil {
		return Credentials{}, apperr.Wrap(err, apperr.IOError, apperr.StageCredentials, "无法读取凭证文件")
	}
	var ff fileFormat
	if err := json.Unmarshal(raw, &ff); err != nil {
		return Credentials{}, apperr.New(apperr.InvalidCredentials, apperr.StageCredentials,
			"凭证文件不是有效的 JSON；如需重置请执行 sph logout 后重新 sph login")
	}
	creds := Credentials{
		Version:        CredentialsVersion,
		SavedAt:        ff.SavedAt,
		Source:         Source(ff.Source),
		VerifiedAt:     ff.VerifiedAt,
		Cookie:         ff.Cookie,
		YuanbaoHeaders: ff.YuanbaoHeaders,
	}
	// Legacy compatibility: missing version/source/verified_at are
	// read as 1 / manual_import / null; an explicit unknown version is fatal.
	if ff.Version != nil {
		if *ff.Version != CredentialsVersion {
			return Credentials{}, apperr.Newf(apperr.InvalidCredentials, apperr.StageCredentials,
				"凭证文件版本 %d 不受支持", *ff.Version)
		}
	}
	if ff.Source == "" {
		creds.Source = SourceManualImport
	}
	if creds.SavedAt.IsZero() {
		creds.SavedAt = info.ModTime()
	}
	if err := creds.Validate(); err != nil {
		return Credentials{}, err
	}
	return creds, nil
}

// Save atomically writes credentials: same-directory temp file created 0600,
// fully written, fsynced, then renamed over the target. Callers
// must hold the modification lock for mutating operations.
func (s *Store) Save(c Credentials) error {
	if err := c.Validate(); err != nil {
		return err
	}
	if c.Version == 0 {
		c.Version = CredentialsVersion
	}
	raw, err := json.MarshalIndent(fileFormat{
		Version:        &c.Version,
		SavedAt:        c.SavedAt,
		Source:         string(c.Source),
		VerifiedAt:     c.VerifiedAt,
		Cookie:         c.Cookie,
		YuanbaoHeaders: c.YuanbaoHeaders,
	}, "", "  ")
	if err != nil {
		return apperr.Wrap(err, apperr.InternalError, apperr.StageCredentials, "凭证序列化失败")
	}
	if len(raw) > MaxCredentialsBytes {
		return apperr.New(apperr.InvalidCredentials, apperr.StageCredentials, "凭证内容超过大小上限")
	}
	if err := os.MkdirAll(s.Dir, 0o700); err != nil {
		return apperr.Wrap(err, apperr.IOError, apperr.StageCredentials, "无法访问配置目录")
	}
	tmp, err := os.CreateTemp(s.Dir, ".credentials-*.tmp")
	if err != nil {
		return apperr.Wrap(err, apperr.IOError, apperr.StageCredentials, "无法创建临时凭证文件")
	}
	tmpName := tmp.Name()
	defer func() {
		if tmpName != "" {
			tmp.Close()
			os.Remove(tmpName)
		}
	}()
	if err := tmp.Chmod(0o600); err != nil {
		return apperr.Wrap(err, apperr.IOError, apperr.StageCredentials, "无法设置临时凭证文件权限")
	}
	if _, err := tmp.Write(raw); err != nil {
		return apperr.Wrap(err, apperr.IOError, apperr.StageCredentials, "写入凭证失败")
	}
	if err := tmp.Sync(); err != nil {
		return apperr.Wrap(err, apperr.IOError, apperr.StageCredentials, "同步凭证失败")
	}
	if err := tmp.Close(); err != nil {
		return apperr.Wrap(err, apperr.IOError, apperr.StageCredentials, "关闭凭证文件失败")
	}
	if err := os.Rename(tmpName, s.Path()); err != nil {
		return apperr.Wrap(err, apperr.IOError, apperr.StageCredentials, "提交凭证文件失败")
	}
	tmpName = "" // committed; nothing to clean up
	return nil
}

// Clear removes the credentials file. Missing file is an idempotent success.
// It never touches anything else in the config directory.
func (s *Store) Clear() error {
	path := s.Path()
	info, err := os.Lstat(path)
	if os.IsNotExist(err) {
		return nil
	}
	if err != nil {
		return apperr.Wrap(err, apperr.IOError, apperr.StageCredentials, "无法访问凭证文件")
	}
	if !info.Mode().IsRegular() {
		return apperr.Newf(apperr.IOError, apperr.StageCredentials,
			"凭证路径不是普通文件，请手动检查 %s", path)
	}
	if err := os.Remove(path); err != nil {
		return apperr.Wrap(err, apperr.IOError, apperr.StageCredentials, "删除凭证文件失败")
	}
	return nil
}

// ParseCookieImport parses manual-import stdin: either a bare
// Cookie value or a single line starting with "Cookie:". Outer whitespace is
// trimmed; CR/LF inside is rejected; whole cURL commands are rejected.
func ParseCookieImport(input string) (string, error) {
	if len(input) > MaxImportBytes {
		return "", apperr.Newf(apperr.InvalidArgument, apperr.StageArguments, "导入内容超过 %d 字节上限", MaxImportBytes)
	}
	trimmed := strings.TrimSpace(input)
	if trimmed == "" {
		return "", apperr.New(apperr.InvalidArgument, apperr.StageArguments, "导入内容为空")
	}
	if strings.ContainsAny(trimmed, "\r\n") {
		return "", apperr.New(apperr.InvalidArgument, apperr.StageArguments, "导入内容包含换行，只接受单行 Cookie 值")
	}
	lower := strings.ToLower(trimmed)
	if strings.HasPrefix(lower, "curl ") || strings.Contains(lower, " -h ") {
		return "", apperr.New(apperr.InvalidArgument, apperr.StageArguments,
			"检测到 cURL 命令；请只粘贴 Cookie 值本身")
	}
	for _, prefix := range []string{"cookie:", "-cookie"} {
		if lower == prefix {
			return "", apperr.New(apperr.InvalidArgument, apperr.StageArguments, "导入内容缺少 Cookie 值")
		}
	}
	if strings.HasPrefix(lower, "cookie:") {
		trimmed = strings.TrimSpace(trimmed[len("cookie:"):])
	}
	if trimmed == "" {
		return "", apperr.New(apperr.InvalidArgument, apperr.StageArguments, "导入内容缺少 Cookie 值")
	}
	if len(trimmed) > MaxCookieBytes {
		return "", apperr.Newf(apperr.InvalidArgument, apperr.StageArguments, "Cookie 超过 %d 字节上限", MaxCookieBytes)
	}
	if !strings.Contains(trimmed, "=") {
		return "", apperr.New(apperr.InvalidArgument, apperr.StageArguments, "Cookie 值应当是 name=value; ... 形式")
	}
	return trimmed, nil
}

// ParseHeadersFile parses the optional --headers-file JSON.
func ParseHeadersFile(data []byte) (map[string]string, error) {
	if len(data) > MaxImportBytes {
		return nil, apperr.Newf(apperr.InvalidArgument, apperr.StageArguments, "额外请求头文件超过 %d 字节上限", MaxImportBytes)
	}
	if len(data) == 0 {
		return nil, nil
	}
	var raw map[string]string
	if err := json.Unmarshal(data, &raw); err != nil {
		return nil, apperr.New(apperr.InvalidArgument, apperr.StageArguments,
			"额外请求头文件必须是 JSON 字符串键值对象")
	}
	for k, v := range raw {
		if strings.ContainsAny(v, "\r\n") {
			return nil, apperr.Newf(apperr.InvalidArgument, apperr.StageArguments, "请求头 %s 的值包含换行符", k)
		}
	}
	return ValidateHeaderMap(raw)
}
