//! 登录检测状态机：对轮询到的 cookie 集合做判定。
//!
//! 登录页会分阶段设置 cookie：访客 cookie 在页面加载时落地，登录流程再添加
//! 会话 cookie。把任何增长都当作"已登录"会在页面还在加载时就误触发。因此：
//!
//!  1. 基线阶段：等待访客 cookie 指纹稳定 watch_stable_polls 轮
//!     （最多 watch_max_baseline_polls 轮后强制锁定，话痨页面不会永久阻塞）；
//!  2. 观察阶段：仅当 cookie 名集合严格增长超过基线（会话 cookie 是新名字）
//!     才触发——值轮换永不触发；
//!  3. 且完整指纹（名+值）随后稳定 watch_stable_polls 轮。

use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CookieEntry {
    pub name: String,
    pub value: String,
}

const WATCH_STABLE_POLLS: usize = 3;
const WATCH_MAX_BASELINE_POLLS: usize = 15;

const PHASE_BASELINE: &str = "baseline";
const PHASE_WATCH: &str = "watch";

#[derive(Debug)]
pub struct LoginWatch {
    phase: &'static str,
    baseline_names: BTreeSet<String>,
    baseline_polls: usize,
    last_fingerprint: String,
    has_last: bool,
    stable_count: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Poll {
    /// 本轮无结论。
    Undecided,
    /// 基线已锁定。
    BaselineLocked,
    /// 检测到登录。
    Detected,
}

impl LoginWatch {
    pub fn new() -> LoginWatch {
        LoginWatch {
            phase: PHASE_BASELINE,
            baseline_names: BTreeSet::new(),
            baseline_polls: 0,
            last_fingerprint: String::new(),
            has_last: false,
            stable_count: 0,
        }
    }

    #[cfg(test)]
    pub fn phase(&self) -> &'static str {
        self.phase
    }

    /// 喂入一轮 cookie，返回本轮判定。
    pub fn poll(&mut self, cookies: &[CookieEntry]) -> Poll {
        let names: BTreeSet<String> = cookies.iter().map(|c| c.name.clone()).collect();
        let fp = cookie_fingerprint(cookies);

        if self.phase == PHASE_BASELINE {
            self.baseline_polls += 1;
            if self.has_last && fp == self.last_fingerprint {
                self.stable_count += 1;
            } else {
                self.stable_count = 0;
            }
            self.last_fingerprint = fp.clone();
            self.has_last = true;
            let settled = self.stable_count >= WATCH_STABLE_POLLS - 1
                && self.baseline_polls >= WATCH_STABLE_POLLS;
            if settled || self.baseline_polls >= WATCH_MAX_BASELINE_POLLS {
                self.baseline_names = names;
                self.phase = PHASE_WATCH;
                self.stable_count = 0;
                self.last_fingerprint = fp.clone();
                return Poll::BaselineLocked;
            }
            return Poll::Undecided;
        }

        let grew = names.iter().any(|n| !self.baseline_names.contains(n));
        if !grew {
            self.last_fingerprint = fp.clone();
            return Poll::Undecided;
        }
        if self.has_last && self.last_fingerprint == fp {
            self.stable_count += 1;
        } else {
            self.last_fingerprint = fp.clone();
            self.stable_count = 1;
        }
        if self.stable_count >= WATCH_STABLE_POLLS {
            return Poll::Detected;
        }
        Poll::Undecided
    }
}

fn cookie_fingerprint(cookies: &[CookieEntry]) -> String {
    let mut parts: Vec<String> = cookies
        .iter()
        .map(|c| format!("{}={}", c.name, c.value))
        .collect();
    parts.sort();
    parts.join("; ")
}

impl Default for LoginWatch {
    fn default() -> LoginWatch {
        LoginWatch::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guest() -> Vec<CookieEntry> {
        vec![
            CookieEntry {
                name: "guest_a".into(),
                value: "1".into(),
            },
            CookieEntry {
                name: "guest_b".into(),
                value: "2".into(),
            },
        ]
    }

    fn with(base: &[CookieEntry], extra: CookieEntry) -> Vec<CookieEntry> {
        let mut out = base.to_vec();
        out.push(extra);
        out
    }

    #[test]
    fn guest_page_loading_is_not_login() {
        let mut w = LoginWatch::new();
        let results = [
            w.poll(&[CookieEntry {
                name: "guest_a".into(),
                value: "1".into(),
            }]),
            w.poll(&guest()),
            w.poll(&with(
                &guest(),
                CookieEntry {
                    name: "guest_c".into(),
                    value: "3".into(),
                },
            )),
        ];
        for (i, r) in results.iter().enumerate() {
            assert_eq!(*r, Poll::Undecided, "poll {}", i + 1);
        }
        assert_eq!(w.phase(), PHASE_BASELINE);
    }

    #[test]
    fn baseline_locks_then_growth_and_stability_detects() {
        let mut w = LoginWatch::new();
        let decisions = [w.poll(&guest()), w.poll(&guest()), w.poll(&guest())];
        assert_eq!(decisions[0], Poll::Undecided);
        assert_eq!(decisions[1], Poll::Undecided);
        assert_eq!(decisions[2], Poll::BaselineLocked);
        assert_eq!(w.phase(), PHASE_WATCH);
        let logged_in = with(
            &guest(),
            CookieEntry {
                name: "session".into(),
                value: "s1".into(),
            },
        );
        assert_eq!(w.poll(&logged_in), Poll::Undecided, "first growth poll");
        assert_eq!(w.poll(&logged_in), Poll::Undecided, "second stable poll");
        assert_eq!(w.poll(&logged_in), Poll::Detected, "third stable poll");
    }

    #[test]
    fn value_rotation_never_triggers() {
        let mut w = LoginWatch::new();
        for _ in 0..WATCH_STABLE_POLLS {
            w.poll(&guest());
        }
        assert_eq!(w.phase(), PHASE_WATCH);
        for i in 0..20 {
            let rotated = vec![
                CookieEntry {
                    name: "guest_a".into(),
                    value: format!("v{}", (b'a' + (i % 26) as u8) as char),
                },
                CookieEntry {
                    name: "guest_b".into(),
                    value: "2".into(),
                },
            ];
            assert_eq!(w.poll(&rotated), Poll::Undecided);
        }
    }

    #[test]
    fn growth_must_settle_before_detecting() {
        let mut w = LoginWatch::new();
        for _ in 0..WATCH_STABLE_POLLS {
            w.poll(&guest());
        }
        assert_eq!(
            w.poll(&with(
                &guest(),
                CookieEntry {
                    name: "s1".into(),
                    value: "a".into()
                }
            )),
            Poll::Undecided
        );
        let settled = with(
            &with(
                &guest(),
                CookieEntry {
                    name: "s1".into(),
                    value: "a".into(),
                },
            ),
            CookieEntry {
                name: "s2".into(),
                value: "b".into(),
            },
        );
        assert_eq!(w.poll(&settled), Poll::Undecided);
        assert_eq!(w.poll(&settled), Poll::Undecided);
        assert_eq!(w.poll(&settled), Poll::Detected);
    }

    #[test]
    fn forces_baseline_after_chatty_page() {
        let mut w = LoginWatch::new();
        let mut decision = Poll::Undecided;
        for i in 0..WATCH_MAX_BASELINE_POLLS {
            if decision == Poll::BaselineLocked {
                break;
            }
            let chatty = vec![
                CookieEntry {
                    name: "guest_a".into(),
                    value: format!("t{}", (b'a' + (i % 26) as u8) as char),
                },
                CookieEntry {
                    name: "guest_b".into(),
                    value: "2".into(),
                },
            ];
            decision = w.poll(&chatty);
        }
        assert_eq!(decision, Poll::BaselineLocked);
        let logged_in = with(
            &guest(),
            CookieEntry {
                name: "session".into(),
                value: "x".into(),
            },
        );
        assert_eq!(w.poll(&logged_in), Poll::Undecided);
        assert_eq!(w.poll(&logged_in), Poll::Undecided);
        assert_eq!(w.poll(&logged_in), Poll::Detected);
    }

    #[test]
    fn empty_cookie_list_tolerated() {
        let mut w = LoginWatch::new();
        for _ in 0..WATCH_STABLE_POLLS {
            w.poll(&[]);
        }
        assert_eq!(w.phase(), PHASE_WATCH);
        let first = vec![CookieEntry {
            name: "first".into(),
            value: "1".into(),
        }];
        for _ in 0..WATCH_STABLE_POLLS - 1 {
            assert_eq!(w.poll(&first), Poll::Undecided);
        }
        assert_eq!(w.poll(&first), Poll::Detected);
    }
}
