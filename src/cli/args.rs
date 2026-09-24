//! 参数解析：严格对照命令的 flag 规格校验。
//! 解析器接受位置参数前 AND 后的 flag、"--flag value"、"--flag=value" 与 "-o value"。

use std::collections::BTreeMap;

use crate::apperr::{AppError, Code, Result, Stage};

#[derive(Debug, Clone, Copy)]
struct FlagSpec {
    name: &'static str,
    short: Option<&'static str>,
    is_bool: bool,
}

const fn spec(name: &'static str) -> FlagSpec {
    FlagSpec {
        name,
        short: None,
        is_bool: false,
    }
}

const fn bool_spec(name: &'static str) -> FlagSpec {
    FlagSpec {
        name,
        short: None,
        is_bool: true,
    }
}

const fn short_spec(name: &'static str, short: &'static str) -> FlagSpec {
    FlagSpec {
        name,
        short: Some(short),
        is_bool: false,
    }
}

fn cmd_flag_specs(command: &str) -> Vec<FlagSpec> {
    match command {
        "login" => vec![spec("timeout"), bool_spec("yuanbao")],
        "publish" => vec![
            spec("title"),
            spec("description"),
            spec("tags"),
            spec("cover"),
            spec("account"),
            spec("at"),
            bool_spec("dry-run"),
            bool_spec("headed"),
            bool_spec("json"),
            spec("timeout"),
        ],
        "logout" => vec![bool_spec("assistant")],
        "history" => vec![spec("limit"), bool_spec("json")],
        "inspect" => vec![bool_spec("stdin"), bool_spec("json"), spec("timeout")],
        "download" => vec![
            bool_spec("stdin"),
            bool_spec("json"),
            spec("timeout"),
            short_spec("output", "o"),
            bool_spec("overwrite"),
            spec("max-bytes"),
        ],
        "auth import" => vec![bool_spec("stdin"), spec("headers-file")],
        _ => vec![],
    }
}

/// 解析结果：规范命令名、按长名索引的 flag、位置参数。
#[derive(Debug, Clone)]
pub struct Command {
    pub command: String,
    pub flags: BTreeMap<String, String>,
    pub positionals: Vec<String>,
}

impl Command {
    pub fn flag(&self, name: &str) -> Option<&str> {
        self.flags.get(name).map(|s| s.as_str())
    }

    pub fn bool_flag(&self, name: &str) -> bool {
        self.flags.contains_key(name)
    }
}

pub fn known_commands() -> Vec<&'static str> {
    vec![
        "login",
        "inspect",
        "download",
        "auth status",
        "auth import",
        "auth clear",
        "logout",
        "publish",
        "accounts",
        "history",
        "version",
        "help",
    ]
}

/// 把 argv 解析成 命令 + flags + 位置参数，严格校验命令的 flag 规格。
pub fn parse_args(argv: &[String]) -> Result<Command> {
    if argv.is_empty() {
        return Ok(Command {
            command: "help".into(),
            flags: BTreeMap::new(),
            positionals: vec![],
        });
    }
    // 任意位置出现的 --help / -h（包括子命令之后）都打印帮助
    for tok in argv {
        if tok == "--help" || tok == "-h" {
            return Ok(Command {
                command: "help".into(),
                flags: BTreeMap::new(),
                positionals: vec![],
            });
        }
    }
    let first = &argv[0];
    match first.as_str() {
        "help" => {
            return Ok(Command {
                command: "help".into(),
                flags: BTreeMap::new(),
                positionals: vec![],
            })
        }
        "--version" | "-v" | "version" => {
            return Ok(Command {
                command: "version".into(),
                flags: BTreeMap::new(),
                positionals: vec![],
            })
        }
        _ => {}
    }
    let mut command = first.clone();
    let mut rest: &[String] = &argv[1..];
    if command == "auth" {
        if rest.is_empty() {
            return Err(AppError::new(
                Code::InvalidArgument,
                Stage::Arguments,
                "auth 需要子命令：status / import / clear",
            ));
        }
        match rest[0].as_str() {
            "status" | "import" | "clear" => {
                command = format!("auth {}", rest[0]);
                rest = &rest[1..];
            }
            other => {
                return Err(AppError::fmt(
                    Code::InvalidArgument,
                    Stage::Arguments,
                    format_args!("未知的 auth 子命令：{other}（可用：status / import / clear）"),
                ))
            }
        }
    }
    if !known_commands().contains(&command.as_str()) {
        return Err(AppError::fmt(
            Code::InvalidArgument,
            Stage::Arguments,
            format_args!(
                "未知命令：{command}（可用：login / inspect / download / auth / logout / version）"
            ),
        ));
    }
    let specs = cmd_flag_specs(&command);
    let mut flags = BTreeMap::new();
    let mut positionals = Vec::new();

    let find_spec = |name: &str| -> Option<FlagSpec> {
        specs
            .iter()
            .find(|s| s.name == name || (s.short.is_some() && s.short.unwrap() == name))
            .copied()
    };

    let mut i = 0;
    while i < rest.len() {
        let tok = &rest[i];
        i += 1;
        if tok == "--" {
            positionals.extend(rest[i..].iter().cloned());
            break;
        }
        if tok.len() > 1 && tok.starts_with('-') {
            let mut name = tok.trim_start_matches('-').to_string();
            let mut value = String::new();
            let mut has_value = false;
            if let Some(eq) = name.find('=') {
                value = name[eq + 1..].to_string();
                name.truncate(eq);
                has_value = true;
            }
            let Some(spec) = find_spec(&name) else {
                return Err(AppError::fmt(
                    Code::InvalidArgument,
                    Stage::Arguments,
                    format_args!("命令 {command} 不支持选项 --{name}"),
                ));
            };
            if spec.is_bool {
                if has_value {
                    if value != "true" && value != "false" {
                        return Err(AppError::fmt(
                            Code::InvalidArgument,
                            Stage::Arguments,
                            format_args!("选项 --{name} 是布尔开关，不接受值"),
                        ));
                    }
                    if value == "false" {
                        flags.remove(spec.name);
                    } else {
                        flags.insert(spec.name.to_string(), "true".to_string());
                    }
                } else {
                    flags.insert(spec.name.to_string(), "true".to_string());
                }
                continue;
            }
            if !has_value {
                if i >= rest.len() {
                    return Err(AppError::fmt(
                        Code::InvalidArgument,
                        Stage::Arguments,
                        format_args!("选项 --{name} 需要一个值"),
                    ));
                }
                value = rest[i].clone();
                i += 1;
            }
            if flags.contains_key(spec.name) {
                return Err(AppError::fmt(
                    Code::InvalidArgument,
                    Stage::Arguments,
                    format_args!("选项 --{name} 重复"),
                ));
            }
            flags.insert(spec.name.to_string(), value);
            continue;
        }
        positionals.push(tok.clone());
    }
    Ok(Command {
        command,
        flags,
        positionals,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(argv: &[&str]) -> Vec<String> {
        argv.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn flag_orders() {
        struct Case {
            argv: Vec<String>,
            want_command: &'static str,
            want_flags: Vec<(&'static str, &'static str)>,
            want_position: Vec<&'static str>,
        }
        let cases = vec![
            Case {
                argv: s(&[
                    "download",
                    "https://weixin.qq.com/sph/a",
                    "-o",
                    "v.mp4",
                    "--json",
                ]),
                want_command: "download",
                want_flags: vec![("output", "v.mp4"), ("json", "true")],
                want_position: vec!["https://weixin.qq.com/sph/a"],
            },
            Case {
                argv: s(&[
                    "download",
                    "-o",
                    "v.mp4",
                    "--json",
                    "https://weixin.qq.com/sph/a",
                ]),
                want_command: "download",
                want_flags: vec![("output", "v.mp4"), ("json", "true")],
                want_position: vec!["https://weixin.qq.com/sph/a"],
            },
            Case {
                argv: s(&["download", "--output=v2.mp4", "--max-bytes=100", "u://x"]),
                want_command: "download",
                want_flags: vec![("output", "v2.mp4"), ("max-bytes", "100")],
                want_position: vec!["u://x"],
            },
            Case {
                argv: s(&["inspect", "u://x", "--timeout", "60s"]),
                want_command: "inspect",
                want_flags: vec![("timeout", "60s")],
                want_position: vec!["u://x"],
            },
            Case {
                argv: s(&["login", "--timeout", "5m"]),
                want_command: "login",
                want_flags: vec![("timeout", "5m")],
                want_position: vec![],
            },
            Case {
                argv: s(&["auth", "import", "--stdin"]),
                want_command: "auth import",
                want_flags: vec![("stdin", "true")],
                want_position: vec![],
            },
            Case {
                argv: s(&["auth", "status"]),
                want_command: "auth status",
                want_flags: vec![],
                want_position: vec![],
            },
            Case {
                argv: s(&["logout"]),
                want_command: "logout",
                want_flags: vec![],
                want_position: vec![],
            },
            Case {
                argv: s(&["auth", "clear"]),
                want_command: "auth clear",
                want_flags: vec![],
                want_position: vec![],
            },
            Case {
                argv: s(&["version"]),
                want_command: "version",
                want_flags: vec![],
                want_position: vec![],
            },
            Case {
                argv: s(&["--help"]),
                want_command: "help",
                want_flags: vec![],
                want_position: vec![],
            },
            Case {
                argv: s(&[]),
                want_command: "help",
                want_flags: vec![],
                want_position: vec![],
            },
        ];
        for c in cases {
            let got = parse_args(&c.argv).unwrap_or_else(|e| panic!("{:?}: {e}", c.argv));
            assert_eq!(got.command, c.want_command, "{:?}", c.argv);
            for (k, v) in &c.want_flags {
                assert_eq!(
                    got.flags.get(*k).map(|x| x.as_str()),
                    Some(*v),
                    "{:?}",
                    c.argv
                );
            }
            assert_eq!(got.positionals, c.want_position, "{:?}", c.argv);
        }
    }

    #[test]
    fn parse_errors() {
        let bad: Vec<Vec<String>> = vec![
            s(&["nope"]),
            s(&["auth"]),
            s(&["auth", "frobnicate"]),
            s(&["download", "--wat", "x"]),
            s(&["download", "-o"]),
            s(&["download", "--json=maybe", "u"]),
            s(&["download", "-o", "a", "-o", "b"]),
            s(&["inspect", "--output", "x"]),
        ];
        for argv in bad {
            let err = parse_args(&argv).expect_err(&format!("{:?}", argv));
            assert_eq!(err.code, Code::InvalidArgument, "{:?}", argv);
        }
        // --json=false 合法，且位置参数可解析
        let cmd = parse_args(&s(&["download", "--json=false", "u"])).unwrap();
        assert!(!cmd.bool_flag("json"));
        assert_eq!(cmd.positionals, vec!["u"]);
        // 两个位置参数解析本身不报错（运行期拒绝）
        let cmd = parse_args(&s(&["download", "u1", "u2"])).unwrap();
        assert_eq!(cmd.positionals.len(), 2);
    }
}
