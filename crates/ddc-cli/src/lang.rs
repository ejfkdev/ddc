//! Bilingual CLI messages. Chinese when the environment asks for it.
//!
//! Resolution: `DDC_LANG` (explicit `zh`/`en` override) > `LC_ALL` >
//! `LC_MESSAGES` > `LANG` > `LANGUAGE` > the Windows user's UI language.
//! A tag selects Chinese when its primary subtag starts with `zh` (zh,
//! zh_CN, zh-Hans, zh_TW.UTF-8, …); any other stated language selects
//! English. `LANGUAGE` is a colon-separated priority list (`zh:en`) —
//! only its first entry counts. `C`, `POSIX` and empty values state no
//! language: the chain keeps walking.
//!
//! Environment variables always win — the Win32 call runs only when no
//! variable stated a language, because plain cmd.exe and PowerShell
//! export no locale variables at all (Git Bash, Cygwin and WSL export
//! `LANG` and are covered by the chain).

use std::sync::OnceLock;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lang {
    En,
    Zh,
}

impl Lang {
    fn detect() -> Lang {
        if let Ok(v) = std::env::var("DDC_LANG") {
            // An explicit override — but only a recognizable one: a typo
            // like DDC_LANG=fr must not pin the language, it falls
            // through to the locale chain.
            if let Some(p) = primary_tag(&v) {
                if p.starts_with("zh") {
                    return Lang::Zh;
                }
                if p.starts_with("en") {
                    return Lang::En;
                }
            }
        }
        for var in ["LC_ALL", "LC_MESSAGES", "LANG", "LANGUAGE"] {
            let Ok(v) = std::env::var(var) else { continue };
            // LANGUAGE ("zh:en") is a priority list; the first entry
            // decides, the rest are fallbacks we do not honor.
            let first = v.split(':').find(|s| !s.is_empty()).unwrap_or("");
            if let Some(p) = primary_tag(first) {
                return if p.starts_with("zh") { Lang::Zh } else { Lang::En };
            }
        }
        // Nothing in the environment stated a language. Plain cmd.exe /
        // PowerShell export no locale variables, so they would always
        // land here — fall back to the Windows user's UI language (the
        // one Windows itself displays). Env vars, when present, always
        // outrank this.
        #[cfg(windows)]
        {
            if let Some(l) = windows_ui_lang() {
                return l;
            }
        }
        Lang::En
    }
}

/// The user's Windows UI language via kernel32. Only Chinese is
/// distinguished; any other UI language keeps the English default.
/// `GetUserDefaultUILanguage` lives in kernel32 (always present in the
/// MSVC link, unlike the getrusage CRT trap of v0.1.18) and returns a
/// LANGID whose low 10 bits are the primary language — 0x04 covers
/// every Chinese variant (zh-CN/zh-TW/zh-HK/…).
#[cfg(windows)]
fn windows_ui_lang() -> Option<Lang> {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetUserDefaultUILanguage() -> u16;
    }
    let langid = unsafe { GetUserDefaultUILanguage() };
    (langid & 0x3ff == 0x04).then_some(Lang::Zh)
}

/// The primary language subtag of a locale tag, lowercased: `zh_CN.UTF-8`
/// → `zh`, `zh-Hans` → `zh`, `en_US` → `en`. The charset (`.UTF-8`) and
/// modifier (`@cjk`) are stripped; `_` and `-` are both accepted as
/// dialect separators. `None` when the tag states no language (empty,
/// `C`, `POSIX`) — the caller keeps walking the variable chain.
fn primary_tag(tag: &str) -> Option<String> {
    let p = tag
        .split(['.', '@'])
        .next()
        .unwrap_or("")
        .split(['_', '-'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if p.is_empty() || p == "c" || p == "posix" {
        None
    } else {
        Some(p)
    }
}

/// The process-wide language, resolved once.
pub(crate) fn lang() -> Lang {
    static LANG: OnceLock<Lang> = OnceLock::new();
    *LANG.get_or_init(Lang::detect)
}

/// Pick one of two static strings by language.
pub(crate) fn pick(en: &'static str, zh: &'static str) -> &'static str {
    match lang() {
        Lang::En => en,
        Lang::Zh => zh,
    }
}

/// `bi!("english", "中文")` → one of the literals.
macro_rules! bi {
    ($en:expr, $zh:expr) => {
        $crate::lang::pick($en, $zh)
    };
}

/// `bif!("{} files", "{} 个文件"; n)` → `format!` of the chosen literal.
/// Both format strings share one argument list — use {0}/{1} positional
/// slots when the argument order differs between the two languages.
/// format! needs a LITERAL first argument, so each arm formats its own
/// captured literal directly (an expr capture would be rejected).
macro_rules! bif {
    ($en:literal, $zh:literal) => {
        $crate::lang::pick($en, $zh)
    };
    ($en:literal, $zh:literal; $($arg:tt)*) => {
        match $crate::lang::lang() {
            $crate::lang::Lang::Zh => format!($zh, $($arg)*),
            $crate::lang::Lang::En => format!($en, $($arg)*),
        }
    };
}

pub(crate) use bi;
pub(crate) use bif;
