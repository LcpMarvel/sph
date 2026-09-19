// Package cli implements argument handling, command dispatch, the output
// contract (plain text vs single JSON object) and exit-code mapping.
package cli

import (
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"strconv"
	"strings"
	"time"

	"github.com/LcpMarvel/sph-downloader/internal/apperr"
	"github.com/LcpMarvel/sph-downloader/internal/auth"
	"github.com/LcpMarvel/sph-downloader/internal/download"
	"github.com/LcpMarvel/sph-downloader/internal/login"
	"github.com/LcpMarvel/sph-downloader/internal/netpolicy"
	"github.com/LcpMarvel/sph-downloader/internal/upstream"
	"github.com/LcpMarvel/sph-downloader/internal/verify"
)

// Version is the tool version reported by `sph version`. Release builds
// inject the tag via -ldflags; source builds report "dev".
var Version = "dev"

// Default command timeouts.
const (
	defaultInspectTimeout  = 60 * time.Second
	defaultDownloadTimeout = 20 * time.Minute
)

// Deps bundles every injectable so tests never touch the network, Node or a
// browser.
type Deps struct {
	ConfigDir   func() string
	Interactive func() bool
	// NewUpstreamClient lets tests point the parse chain at fake servers.
	NewUpstreamClient func() *upstream.Client
	// NewMediaClient lets tests fake media servers.
	NewMediaClient func() download.HTTPClient
	Now            func() time.Time
}

// DefaultDeps wires production implementations.
func DefaultDeps() Deps {
	return Deps{
		ConfigDir:   auth.DefaultConfigDir,
		Interactive: func() bool { return stdinIsTerminal() },
		NewUpstreamClient: func() *upstream.Client {
			return upstream.NewClient(netpolicy.NewAPIClient())
		},
		NewMediaClient: func() download.HTTPClient { return netpolicy.NewMediaClient() },
		Now:            time.Now,
	}
}

// Run executes one command invocation and returns the process exit code.
// stdout receives exactly one final payload (or one JSON object in --json
// mode); everything else goes to stderr.
func Run(ctx context.Context, argv []string, stdin io.Reader, stdout, stderr io.Writer, deps Deps) int {
	cmd, aerr := parseArgs(argv)
	if aerr != nil {
		json := false
		if cmd != nil {
			json = cmd.boolFlag("json")
		}
		return fail(aerr, json, stdout, stderr)
	}
	jsonMode := cmd.boolFlag("json")
	err := dispatch(ctx, cmd, stdin, stdout, stderr, deps)
	if ctx.Err() != nil && err != nil && apperr.CodeOf(err) != apperr.Cancelled {
		// outer cancellation (Ctrl+C) wins over any wrapped cause
		err = apperr.New(apperr.Cancelled, apperr.StageOf(err), "操作已取消")
	}
	if err != nil {
		return fail(apperr.From(err), jsonMode, stdout, stderr)
	}
	return 0
}

func dispatch(ctx context.Context, cmd *commandWithFlags, stdin io.Reader, stdout, stderr io.Writer, deps Deps) error {
	switch cmd.Command {
	case "help":
		fmt.Fprint(stdout, helpText)
		return nil
	case "version":
		fmt.Fprintf(stdout, "sph %s\n", Version)
		return nil
	case "login":
		return runLogin(ctx, cmd, stdin, stdout, stderr, deps)
	case "auth status":
		return runAuthStatus(stdout, deps)
	case "auth import":
		return runAuthImport(cmd, stdin, stdout, deps)
	case "logout", "auth clear":
		return runLogout(stdout, deps)
	case "inspect":
		return runInspect(ctx, cmd, stdin, stdout, stderr, deps)
	case "download":
		return runDownload(ctx, cmd, stdin, stdout, stderr, deps)
	default:
		return apperr.Newf(apperr.InvalidArgument, apperr.StageArguments, "未知命令：%s", cmd.Command)
	}
}

// --- URL input handling ---------------------------------------------------

// inputURL returns the single share URL from positionals or --stdin
// .
func inputURL(cmd *commandWithFlags, stdin io.Reader) (string, *apperr.Error) {
	useStdin := cmd.boolFlag("stdin")
	if useStdin && len(cmd.Positionals) > 0 {
		return "", apperr.New(apperr.InvalidArgument, apperr.StageArguments,
			"--stdin 与位置参数 URL 不能同时使用")
	}
	if useStdin {
		data, err := io.ReadAll(io.LimitReader(stdin, 64<<10+1))
		if err != nil {
			return "", apperr.New(apperr.InvalidArgument, apperr.StageArguments, "读取 stdin 失败")
		}
		if len(data) > 64<<10 {
			return "", apperr.New(apperr.InvalidArgument, apperr.StageArguments, "stdin 输入超过 64 KiB 上限")
		}
		text := strings.TrimSpace(string(data))
		if text == "" {
			return "", apperr.New(apperr.InvalidArgument, apperr.StageArguments, "stdin 输入为空")
		}
		if strings.ContainsAny(text, "\n\r") {
			return "", apperr.New(apperr.InvalidArgument, apperr.StageArguments, "stdin 输入包含多个链接")
		}
		return text, nil
	}
	switch len(cmd.Positionals) {
	case 0:
		return "", apperr.New(apperr.InvalidArgument, apperr.StageArguments, "缺少分享链接参数")
	case 1:
		return cmd.Positionals[0], nil
	default:
		return "", apperr.New(apperr.InvalidArgument, apperr.StageArguments, "一次只支持一个链接")
	}
}

func parseTimeoutFlag(cmd *commandWithFlags, def time.Duration) (time.Duration, *apperr.Error) {
	raw, ok := cmd.flag("timeout")
	if !ok {
		return def, nil
	}
	d, err := time.ParseDuration(raw)
	if err != nil || d <= 0 {
		return 0, apperr.Newf(apperr.InvalidArgument, apperr.StageArguments,
			"--timeout 必须是正的时长（如 60s、5m）")
	}
	return d, nil
}

// --- inspect / download ----------------------------------------------------

func loadCredentials(deps Deps) (auth.Credentials, *auth.Store, error) {
	store, err := auth.NewStore(deps.ConfigDir())
	if err != nil {
		return auth.Credentials{}, nil, err
	}
	creds, err := store.Load()
	if err != nil {
		if errors.Is(err, auth.ErrNoCredentials) {
			return auth.Credentials{}, store, apperr.New(apperr.AuthRequired, apperr.StageCredentials,
				"未找到登录凭证，请执行 sph login 重新登录。")
		}
		return auth.Credentials{}, store, err
	}
	return creds, store, nil
}

func runInspect(ctx context.Context, cmd *commandWithFlags, stdin io.Reader, stdout, stderr io.Writer, deps Deps) error {
	rawURL, aerr := inputURL(cmd, stdin)
	if aerr != nil {
		return aerr
	}
	timeout, aerr := parseTimeoutFlag(cmd, defaultInspectTimeout)
	if aerr != nil {
		return aerr
	}
	creds, _, err := loadCredentials(deps)
	if err != nil {
		return err
	}
	ctx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()
	client := deps.NewUpstreamClient()
	video, err := client.Resolve(ctx, rawURL, creds)
	if err != nil {
		return err
	}
	result := video.Inspect()
	if cmd.boolFlag("json") {
		return writeJSON(stdout, envelope{OK: true, Command: "inspect", Data: result})
	}
	fmt.Fprintf(stdout, "本地标识: %s\n", result.LocalID)
	fmt.Fprintf(stdout, "标题: %s\n", result.Title)
	fmt.Fprintf(stdout, "作者: %s\n", result.Author)
	fmt.Fprintf(stdout, "媒体来源: %s\n", result.MediaSource)
	fmt.Fprintf(stdout, "编码提示: %s\n", result.CodecHint)
	return nil
}

func runDownload(ctx context.Context, cmd *commandWithFlags, stdin io.Reader, stdout, stderr io.Writer, deps Deps) error {
	rawURL, aerr := inputURL(cmd, stdin)
	if aerr != nil {
		return aerr
	}
	timeout, aerr := parseTimeoutFlag(cmd, defaultDownloadTimeout)
	if aerr != nil {
		return aerr
	}
	maxBytes := int64(download.DefaultMaxBytes)
	if raw, ok := cmd.flag("max-bytes"); ok {
		v, err := parseInt64(raw)
		if err != nil || v <= 0 {
			return apperr.New(apperr.InvalidArgument, apperr.StageArguments,
				"--max-bytes 必须是正整数")
		}
		maxBytes = v
	}
	creds, _, err := loadCredentials(deps)
	if err != nil {
		return err
	}
	ctx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()

	client := deps.NewUpstreamClient()
	video, err := client.Resolve(ctx, rawURL, creds)
	if err != nil {
		return err
	}

	output, _ := cmd.flag("output")
	opts := download.Options{
		OutputPath: output,
		WorkDir:    cwdOrNull(),
		Overwrite:  cmd.boolFlag("overwrite"),
		MaxBytes:   maxBytes,
		HTTP:       deps.NewMediaClient(),
		Progress:   newProgressPrinter(stderr),
		Warn:       func(msg string) { fmt.Fprintf(stderr, "警告：%s\n", msg) },
	}
	dl, err := download.New(video, opts)
	if err != nil {
		return err
	}
	result, err := dl.Run(ctx)
	if err != nil {
		return err
	}
	if result.Verification == verify.VerificationContainer {
		fmt.Fprintln(stderr, "提示：未安装 ffprobe，仅完成容器基础检查，尚未验证视频流与播放。")
	}
	if cmd.boolFlag("json") {
		return writeJSON(stdout, envelope{OK: true, Command: "download", Data: result})
	}
	fmt.Fprintln(stdout, result.Path)
	fmt.Fprintf(stderr, "已保存 %d 字节（SHA-256 %s，验证方式 %s）\n", result.Bytes, result.SHA256, result.Verification)
	return nil
}

func parseInt64(s string) (int64, error) {
	v, err := strconv.ParseInt(s, 10, 64)
	return v, err
}

func cwdOrNull() string {
	dir, err := os.Getwd()
	if err != nil {
		return ""
	}
	return dir
}

// --- auth commands ----------------------------------------------------------

func runAuthStatus(stdout io.Writer, deps Deps) error {
	store, err := auth.NewStore(deps.ConfigDir())
	if err != nil {
		return err
	}
	if !store.Exists() {
		fmt.Fprintln(stdout, "状态: 未配置凭证")
		fmt.Fprintf(stdout, "配置目录: %s\n", store.Dir)
		fmt.Fprintln(stdout, "如需登录请执行: sph login")
		return nil
	}
	fmt.Fprintf(stdout, "状态: 已保存本地凭证\n")
	fmt.Fprintf(stdout, "配置目录: %s\n", store.Dir)
	fmt.Fprintf(stdout, "凭证文件: %s\n", store.Path())
	creds, err := store.Load()
	if err != nil {
		fmt.Fprintln(stdout, "读取详情失败: 凭证文件存在但未通过校验")
		return nil
	}
	fmt.Fprintf(stdout, "来源: %s\n", string(creds.Source))
	fmt.Fprintf(stdout, "保存时间: %s\n", creds.SavedAt.UTC().Format(time.RFC3339))
	if creds.VerifiedAt != nil {
		fmt.Fprintf(stdout, "上次验证: %s\n", creds.VerifiedAt.UTC().Format(time.RFC3339))
	} else {
		fmt.Fprintln(stdout, "上次验证: 未验证")
	}
	if len(creds.YuanbaoHeaders) > 0 {
		names := make([]string, 0, len(creds.YuanbaoHeaders))
		for k := range creds.YuanbaoHeaders {
			names = append(names, k)
		}
		sortStrings(names)
		fmt.Fprintf(stdout, "额外请求头: %s\n", strings.Join(names, ", "))
	} else {
		fmt.Fprintln(stdout, "额外请求头: 无")
	}
	fmt.Fprintln(stdout, "（以上仅为上次验证时间，不代表当前必然有效）")
	return nil
}

func runAuthImport(cmd *commandWithFlags, stdin io.Reader, stdout io.Writer, deps Deps) error {
	if !cmd.boolFlag("stdin") {
		return apperr.New(apperr.InvalidArgument, apperr.StageArguments,
			"auth import 需要 --stdin（例如 pbpaste | sph auth import --stdin）")
	}
	if len(cmd.Positionals) > 0 {
		return apperr.New(apperr.InvalidArgument, apperr.StageArguments, "auth import 不接受位置参数")
	}
	data, err := io.ReadAll(io.LimitReader(stdin, 64<<10+1))
	if err != nil || len(data) > 64<<10 {
		return apperr.New(apperr.InvalidArgument, apperr.StageArguments, "读取 stdin 失败或超过 64 KiB 上限")
	}
	cookie, aerr := auth.ParseCookieImport(string(data))
	if aerr != nil {
		return aerr
	}
	headers := map[string]string{}
	if hf, ok := cmd.flag("headers-file"); ok {
		raw, err := os.ReadFile(hf)
		if err != nil {
			return apperr.New(apperr.InvalidArgument, apperr.StageArguments, "无法读取额外请求头文件")
		}
		headers, aerr = auth.ParseHeadersFile(raw)
		if aerr != nil {
			return aerr
		}
	}
	store, err := auth.NewStore(deps.ConfigDir())
	if err != nil {
		return err
	}
	lock, err := store.AcquireLock()
	if err != nil {
		return err
	}
	defer lock.Release()
	now := deps.Now().UTC()
	creds := auth.Credentials{
		Version:        auth.CredentialsVersion,
		SavedAt:        now,
		Source:         auth.SourceManualImport,
		VerifiedAt:     nil, // import only proves format, not validity
		Cookie:         cookie,
		YuanbaoHeaders: headers,
	}
	if err := store.Save(creds); err != nil {
		return err
	}
	fmt.Fprintln(stdout, "已导入，尚未验证；首次使用时会通过解析链验证。")
	return nil
}

func runLogout(stdout io.Writer, deps Deps) error {
	store, err := auth.NewStore(deps.ConfigDir())
	if err != nil {
		return err
	}
	lock, err := store.AcquireLock()
	if err != nil {
		return err
	}
	defer lock.Release()
	if err := store.Clear(); err != nil {
		return err
	}
	fmt.Fprintln(stdout, "已清除本工具保存的本地凭证（不影响浏览器登录）。")
	return nil
}

// --- login ------------------------------------------------------------------

func runLogin(ctx context.Context, cmd *commandWithFlags, stdin io.Reader, stdout, stderr io.Writer, deps Deps) error {
	if cmd.boolFlag("json") {
		return apperr.New(apperr.InvalidArgument, apperr.StageArguments, "login 不支持 --json")
	}
	if len(cmd.Positionals) > 0 {
		return apperr.New(apperr.InvalidArgument, apperr.StageArguments, "login 不接受位置参数")
	}
	timeout, aerr := parseTimeoutFlag(cmd, login.DefaultTimeout)
	if aerr != nil {
		return aerr
	}
	store, err := auth.NewStore(deps.ConfigDir())
	if err != nil {
		return err
	}
	opts := login.Options{
		Timeout:       timeout,
		Stdin:         stdin,
		Stderr:        stderr,
		IsInteractive: deps.Interactive,
		Now:           deps.Now,
	}
	return login.Run(ctx, store, opts)
}
