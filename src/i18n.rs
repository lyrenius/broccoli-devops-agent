//! Output language of the agent: everything the control plane writes for humans — event
//! summaries, denial rationales, verification and execution summaries, Team diagnoses, and the
//! feedback text the model reads — comes out in one language, chosen in the config file at
//! startup. It is a process-wide setting on purpose: the log must not switch language halfway
//! through an incident, and replayed transcripts must read as they were written. The consoles
//! translate their own chrome at runtime independently of this.

use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

/// Languages the agent can write in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Language {
    /// English.
    #[default]
    #[serde(rename = "en", alias = "en-US", alias = "english")]
    En,
    /// Simplified Chinese.
    #[serde(
        rename = "zh-CN",
        alias = "zh-cn",
        alias = "zh",
        alias = "zh-Hans",
        alias = "chinese"
    )]
    ZhCn,
}

impl Language {
    /// The BCP 47 tag, as served to the consoles.
    pub fn tag(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::ZhCn => "zh-CN",
        }
    }

    /// The instruction appended to model prompts so model-written text follows the setting.
    ///
    /// Identifiers stay as they are: runbook IDs, resource IDs, and JSON field names are
    /// protocol, not prose.
    pub fn model_instruction(self) -> &'static str {
        match self {
            Self::En => "",
            Self::ZhCn => {
                "\nLanguage: write the diagnosis summary, every unresolved question, and every \
                 proposal's reason and expected_effect in Simplified Chinese (简体中文). Keep \
                 runbook IDs, resource IDs, and JSON field names exactly as given."
            }
        }
    }
}

static LANGUAGE: AtomicU8 = AtomicU8::new(0);

/// Sets the process-wide output language. Called once at startup from the config.
pub fn set_language(language: Language) {
    LANGUAGE.store(
        match language {
            Language::En => 0,
            Language::ZhCn => 1,
        },
        Ordering::Relaxed,
    );
}

/// The process-wide output language; English until `set_language` is called.
pub fn language() -> Language {
    match LANGUAGE.load(Ordering::Relaxed) {
        1 => Language::ZhCn,
        _ => Language::En,
    }
}

/// Whether the agent writes Simplified Chinese.
pub fn is_zh() -> bool {
    language() == Language::ZhCn
}

/// Picks the text for the current language.
///
/// Both arms are ordinary expressions (usually `format!` calls), so every message keeps its
/// arguments type-checked in both languages and the translations sit next to the code that
/// emits them.
#[macro_export]
macro_rules! tr {
    ($en:expr, $zh:expr $(,)?) => {
        if $crate::i18n::is_zh() { $zh } else { $en }
    };
}
