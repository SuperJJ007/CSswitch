#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptKind {
    Password,
    KeyPassword,
    VerificationCode,
    Unknown,
}

#[allow(dead_code)]
pub fn classify_prompt(prompt: &str) -> PromptKind {
    let lower = prompt.to_lowercase();
    let verification_words = [
        "one-time",
        "one time",
        "otp",
        "verification",
        "passcode",
        "token",
        "2fa",
        "mfa",
        "two-factor",
        "multi-factor",
        "duo",
        "动态",
        "一次性",
        "验证码",
        "令牌",
        "双因素",
        "多因素",
        "短信验证",
        "手机验证",
    ];
    if verification_words.iter().any(|word| lower.contains(word)) {
        return PromptKind::VerificationCode;
    }
    if lower.contains("passphrase") || prompt.contains("密钥密码") {
        return PromptKind::KeyPassword;
    }
    if lower.contains("password") || prompt.contains("密码") || prompt.contains("口令") {
        return PromptKind::Password;
    }
    PromptKind::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn otp_like_prompts_are_not_password_prompts() {
        for prompt in [
            "One-time password:",
            "Verification code:",
            "Duo passcode:",
            "Enter token:",
            "请输入动态验证码",
            "短信验证",
            "双因素认证",
        ] {
            assert_eq!(classify_prompt(prompt), PromptKind::VerificationCode);
        }
    }

    #[test]
    fn password_prompts_are_recognized_after_otp_words_are_excluded() {
        assert_eq!(
            classify_prompt("ubuntu@example.com's password:"),
            PromptKind::Password
        );
        assert_eq!(classify_prompt("Password:"), PromptKind::Password);
        assert_eq!(classify_prompt("请输入密码"), PromptKind::Password);
    }
}
