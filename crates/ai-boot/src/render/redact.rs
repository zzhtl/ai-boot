//! 发卡前的密钥脱敏。
//!
//! Agent 读得到代码、Jira 附件和聊天记录，里面难免夹着 token 或密码；结论发在群里，
//! 所以在渲染前统一抹掉。这是兜底，不替代「Agent 读不到本机凭据文件」的隔离。

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

struct Rule {
    pattern: Regex,
    replacement: &'static str,
}

static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    [
        // 私钥块整段抹掉
        (
            r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z0-9 ]*PRIVATE KEY-----",
            "[已脱敏：私钥]",
        ),
        (r"\bglpat-[0-9A-Za-z_\-]{20,}", "glpat-[已脱敏]"),
        (r"\bsk-ant-[0-9A-Za-z_\-]{20,}", "sk-ant-[已脱敏]"),
        (r"\bsk-[0-9A-Za-z]{32,}", "sk-[已脱敏]"),
        (r"\bAKIA[0-9A-Z]{16}\b", "AKIA[已脱敏]"),
        // 飞书的 tenant/user access token
        (r"\b[tu]-[0-9A-Za-z_\-]{30,}", "[已脱敏：飞书 token]"),
        (r"(?i)\bbearer\s+[0-9A-Za-z._\-]{16,}", "Bearer [已脱敏]"),
        // key=value / key: value 形式的口令与令牌。关键字前不要求词边界：
        // `DB_PASSWORD` 里的下划线也是单词字符；关键字后必须紧跟分隔符，
        // 所以 `tokens: 5` 这类不会被误伤，多抹一点也可以接受
        (
            r#"(?i)(password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key)(["']?\s*[:=]\s*["']?)[^\s"',;]+"#,
            "${1}${2}[已脱敏]",
        ),
    ]
    .into_iter()
    .filter_map(|(pattern, replacement)| {
        // 规则是编译期常量，测试覆盖了每一条；真编译失败也只是少一条规则
        Regex::new(pattern)
            .ok()
            .map(|pattern| Rule {
                pattern,
                replacement,
            })
    })
    .collect()
});

pub fn redact(text: &str) -> Cow<'_, str> {
    let mut out = Cow::Borrowed(text);
    for rule in RULES.iter() {
        if let Cow::Owned(replaced) = rule.pattern.replace_all(&out, rule.replacement) {
            out = Cow::Owned(replaced);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_rule_compiles() {
        assert_eq!(RULES.len(), 8);
    }

    #[test]
    fn common_secrets_are_masked() {
        let cases = [
            (
                "git clone https://oauth2:glpat-AbCdEfGhIjKlMnOpQrSt12@host",
                "glpat-",
            ),
            ("key sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAA", "sk-ant-"),
            ("AKIAIOSFODNN7EXAMPLE", "AKIA"),
            (
                "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.abcdefgh",
                "Bearer",
            ),
            ("t-g1044ghJ4ZyQ7GHP2QFGGCNRKY4B2FVHKXXXXXXX", "飞书 token"),
        ];
        for (input, keep) in cases {
            let out = redact(input);
            assert!(out.contains("[已脱敏"), "{input} → {out}");
            assert!(out.contains(keep), "{input} → {out}");
        }
    }

    #[test]
    fn a_token_under_a_key_is_masked_either_way() {
        // 先被 GitLab 规则抹掉，再被 key=value 规则整段抹掉：结果里只剩键名
        let out = redact("token: glpat-AbCdEfGhIjKlMnOpQrSt12");
        assert!(!out.contains("AbCd"), "{out}");
        assert!(out.starts_with("token: [已脱敏"), "{out}");
    }

    #[test]
    fn key_value_credentials_keep_the_key_and_drop_the_value() {
        assert_eq!(redact("password=hunter2"), "password=[已脱敏]");
        assert_eq!(redact(r#""api_key": "abc123""#), r#""api_key": "[已脱敏]""#);
        assert_eq!(redact("DB_PASSWORD: s3cr3t"), "DB_PASSWORD: [已脱敏]");
    }

    #[test]
    fn private_keys_are_removed_whole() {
        let pem =
            "前\n-----BEGIN RSA PRIVATE KEY-----\nMIIE...\nabc\n-----END RSA PRIVATE KEY-----\n后";
        assert_eq!(redact(pem), "前\n[已脱敏：私钥]\n后");
    }

    #[test]
    fn ordinary_text_is_untouched_and_not_copied() {
        let text = "根因是连接池耗尽，token 刷新逻辑没有问题。";
        assert!(matches!(redact(text), Cow::Borrowed(_)));
    }
}
