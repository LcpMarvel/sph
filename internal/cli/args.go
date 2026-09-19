package cli

import (
	"strings"

	"github.com/LcpMarvel/sph-downloader/internal/apperr"
)

// flagSpec describes one accepted flag for a command.
type flagSpec struct {
	name   string // canonical long name, e.g. "output"
	short  string // optional short alias, e.g. "o"
	isBool bool
}

var (
	cmdFlagSpecs = map[string][]flagSpec{
		"login": {
			{name: "timeout"},
		},
		"inspect": {
			{name: "stdin", isBool: true}, {name: "json", isBool: true}, {name: "timeout"},
		},
		"download": {
			{name: "stdin", isBool: true}, {name: "json", isBool: true}, {name: "timeout"},
			{name: "output", short: "o"}, {name: "overwrite", isBool: true}, {name: "max-bytes"},
		},
		"auth import": {
			{name: "stdin", isBool: true}, {name: "headers-file"},
		},
	}
)

// commandWithFlags is the parse result: canonical command, flags by long
// name, and positional arguments. The parser accepts flags before AND after
// positional arguments, "--flag value", "--flag=value" and
// "-o value".
type commandWithFlags struct {
	Command     string
	Flags       map[string]string
	Positionals []string
}

func (c *commandWithFlags) flag(name string) (string, bool) {
	v, ok := c.Flags[name]
	return v, ok
}

func (c *commandWithFlags) boolFlag(name string) bool {
	_, ok := c.Flags[name]
	return ok
}

var knownCommands = map[string]bool{
	"login": true, "inspect": true, "download": true,
	"auth status": true, "auth import": true, "auth clear": true,
	"logout": true, "version": true, "help": true,
}

// parseArgs tokenizes argv into command + flags + positionals with strict
// validation against the command's flag spec.
func parseArgs(argv []string) (*commandWithFlags, *apperr.Error) {
	if len(argv) == 0 {
		return &commandWithFlags{Command: "help", Flags: map[string]string{}}, nil
	}
	// --help / -h anywhere up front (including after a subcommand) prints help.
	for _, tok := range argv {
		if tok == "--help" || tok == "-h" {
			return &commandWithFlags{Command: "help", Flags: map[string]string{}}, nil
		}
	}
	// First token: command (or a global help/version flag).
	first := argv[0]
	switch first {
	case "--help", "-h", "help":
		return &commandWithFlags{Command: "help", Flags: map[string]string{}}, nil
	case "--version", "-v":
		return &commandWithFlags{Command: "version", Flags: map[string]string{}}, nil
	}
	command := first
	rest := argv[1:]
	if command == "auth" {
		if len(rest) == 0 {
			return nil, apperr.New(apperr.InvalidArgument, apperr.StageArguments,
				"auth 需要子命令：status / import / clear")
		}
		switch rest[0] {
		case "status", "import", "clear":
			command = "auth " + rest[0]
			rest = rest[1:]
		default:
			return nil, apperr.Newf(apperr.InvalidArgument, apperr.StageArguments,
				"未知的 auth 子命令：%s（可用：status / import / clear）", rest[0])
		}
	}
	if !knownCommands[command] {
		return nil, apperr.Newf(apperr.InvalidArgument, apperr.StageArguments,
			"未知命令：%s（可用：login / inspect / download / auth / logout / version）", command)
	}
	specs := cmdFlagSpecs[command]
	flags := map[string]string{}
	var positionals []string

	findSpec := func(name string) *flagSpec {
		for i := range specs {
			if specs[i].name == name || (specs[i].short != "" && specs[i].short == name) {
				return &specs[i]
			}
		}
		return nil
	}

	i := 0
	for i < len(rest) {
		tok := rest[i]
		i++
		if tok == "--" {
			positionals = append(positionals, rest[i:]...)
			break
		}
		if len(tok) > 1 && strings.HasPrefix(tok, "-") {
			name := strings.TrimPrefix(strings.TrimPrefix(tok, "--"), "-")
			value := ""
			hasValue := false
			if eq := strings.IndexByte(name, '='); eq >= 0 {
				name, value, hasValue = name[:eq], name[eq+1:], true
			}
			spec := findSpec(name)
			if spec == nil {
				return nil, apperr.Newf(apperr.InvalidArgument, apperr.StageArguments,
					"命令 %s 不支持选项 --%s", command, name)
			}
			if spec.isBool {
				if hasValue {
					if value != "true" && value != "false" {
						return nil, apperr.Newf(apperr.InvalidArgument, apperr.StageArguments,
							"选项 --%s 是布尔开关，不接受值", name)
					}
					if value == "false" {
						delete(flags, spec.name)
					} else {
						flags[spec.name] = "true"
					}
				} else {
					flags[spec.name] = "true"
				}
				continue
			}
			if !hasValue {
				if i >= len(rest) {
					return nil, apperr.Newf(apperr.InvalidArgument, apperr.StageArguments,
						"选项 --%s 需要一个值", name)
				}
				value = rest[i]
				i++
			}
			if _, dup := flags[spec.name]; dup {
				return nil, apperr.Newf(apperr.InvalidArgument, apperr.StageArguments,
					"选项 --%s 重复", name)
			}
			flags[spec.name] = value
			continue
		}
		positionals = append(positionals, tok)
	}
	return &commandWithFlags{Command: command, Flags: flags, Positionals: positionals}, nil
}
