//! The speech models voice input offers: GGUF builds for transcribe.cpp,
//! published by handy-computer on Hugging Face, each pinned to a commit and
//! checked by its SHA-256 (the list T3 Code ships, `@handy-computer/models`
//! 0.2.3, cut down to the useful picks). Accuracy is the share of English
//! words a model gets wrong on its benchmark; speed is seconds of work for
//! 30 seconds of speech on an Apple GPU.

use crate::ModelFile;

pub struct SpeechModel {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub file: ModelFile,
    pub languages: &'static [&'static str],
    pub recommended: bool,
    pub english_error: Option<f32>,
    pub seconds_per_30s: Option<f32>,
}

/// The model offered first: the best all-round English one.
pub const DEFAULT_MODEL: &str = "parakeet-en";

/// Whisper's languages.
const WHISPER: &[&str] = &[
    "af", "am", "ar", "as", "az", "ba", "be", "bg", "bn", "bo", "br", "bs", "ca", "cs", "cy", "da",
    "de", "el", "en", "es", "et", "eu", "fa", "fi", "fo", "fr", "gl", "gu", "haw", "ha", "he",
    "hi", "hr", "ht", "hu", "hy", "id", "is", "it", "ja", "jw", "ka", "kk", "km", "kn", "ko", "la",
    "lb", "ln", "lo", "lt", "lv", "mg", "mi", "mk", "ml", "mn", "mr", "ms", "mt", "my", "ne", "nl",
    "nn", "no", "oc", "pa", "pl", "ps", "pt", "ro", "ru", "sa", "sd", "si", "sk", "sl", "sn", "so",
    "sq", "sr", "su", "sv", "sw", "ta", "te", "tg", "th", "tk", "tl", "tr", "tt", "uk", "ur", "uz",
    "vi", "yi", "yo", "zh",
];

pub const MODELS: &[SpeechModel] = &[
    SpeechModel {
        id: "parakeet-en",
        name: "Parakeet Unified English 0.6B",
        description: "Best all-around choice for English dictation",
        file: ModelFile {
            name: "parakeet-unified-en-0.6b-Q8_0.gguf",
            url: "https://huggingface.co/handy-computer/parakeet-unified-en-0.6b-gguf/resolve/7e948f21b7bdbac698d3318db9d350f1096f3b6c/parakeet-unified-en-0.6b-Q8_0.gguf",
            sha256: "4b50b6dd862bf6e346929aaf4f5eaacec003bfa3f56462d6c874b41ef2f38795",
            size: 731357568,
        },
        languages: &["en"],
        recommended: true,
        english_error: Some(4.0),
        seconds_per_30s: Some(1.19),
    },
    SpeechModel {
        id: "nemotron-3.5",
        name: "Nemotron Streaming 3.5",
        description: "Fast and accurate transcription in many European languages",
        file: ModelFile {
            name: "nemotron-3.5-asr-streaming-0.6b-Q8_0.gguf",
            url: "https://huggingface.co/handy-computer/nemotron-3.5-asr-streaming-0.6b-gguf/resolve/6d44e540bc31b0de1dbe174a3cea87f53a7f22fb/nemotron-3.5-asr-streaming-0.6b-Q8_0.gguf",
            sha256: "b94545b313b3223fda7b2857a52681da813935c2127643d1e9ff0c23d988089c",
            size: 751094240,
        },
        languages: &[
            "en", "es", "fr", "it", "pt", "nl", "de", "tr", "ru", "ar", "hi", "ja", "ko", "vi",
            "uk", "pl", "sv", "cs", "nb", "da", "bg", "fi", "hr", "sk", "zh", "hu", "ro", "et",
        ],
        recommended: true,
        english_error: Some(7.9),
        seconds_per_30s: Some(1.75),
    },
    SpeechModel {
        id: "canary-180m",
        name: "Canary 180M Flash",
        description: "Tiny, fast, and runs well on any hardware",
        file: ModelFile {
            name: "canary-180m-flash-Q8_0.gguf",
            url: "https://huggingface.co/handy-computer/canary-180m-flash-gguf/resolve/46b86384d1db59108d6e060a0fc00924cb58499f/canary-180m-flash-Q8_0.gguf",
            sha256: "f942ab746040b531a29ab78ca3afc8dc9564efd8e4c942b5541e53644be9c408",
            size: 218447648,
        },
        languages: &["en", "de", "es", "fr"],
        recommended: true,
        english_error: Some(6.0),
        seconds_per_30s: Some(0.91),
    },
    SpeechModel {
        id: "cohere-transcribe",
        name: "Cohere Transcribe",
        description: "Consistently strong in many languages, best for Greek and Polish",
        file: ModelFile {
            name: "cohere-transcribe-03-2026-Q8_0.gguf",
            url: "https://huggingface.co/handy-computer/cohere-transcribe-03-2026-gguf/resolve/dfa4adebb64f3076b7b6b90b721275cc069cb421/cohere-transcribe-03-2026-Q8_0.gguf",
            sha256: "931916663432fd895423a4291a8400221802b288967ca2d435fc5e3141c9e71e",
            size: 2410655232,
        },
        languages: &[
            "en", "fr", "de", "es", "it", "pt", "nl", "pl", "el", "ar", "ja", "zh", "vi", "ko",
        ],
        recommended: true,
        english_error: Some(5.1),
        seconds_per_30s: Some(3.71),
    },
    SpeechModel {
        id: "whisper-large-v3-turbo",
        name: "Whisper Large v3 Turbo",
        description: "Accurate in many languages. Slow without a GPU",
        file: ModelFile {
            name: "whisper-large-v3-turbo-Q8_0.gguf",
            url: "https://huggingface.co/handy-computer/whisper-large-v3-turbo-gguf/resolve/5eaf945c7978e564bae5b28a5b1639dd93c2bfb1/whisper-large-v3-turbo-Q8_0.gguf",
            sha256: "b2e30cc286bc9f3aba4db9099fc7403543497c05ce7100d0d83091ddfd25a183",
            size: 886381760,
        },
        languages: &[
            "af", "am", "ar", "as", "az", "ba", "be", "bg", "bn", "bo", "br", "bs", "ca", "cs",
            "cy", "da", "de", "el", "en", "es", "et", "eu", "fa", "fi", "fo", "fr", "gl", "gu",
            "haw", "ha", "he", "hi", "hr", "ht", "hu", "hy", "id", "is", "it", "ja", "jw", "ka",
            "kk", "km", "kn", "ko", "la", "lb", "ln", "lo", "lt", "lv", "mg", "mi", "mk", "ml",
            "mn", "mr", "ms", "mt", "my", "ne", "nl", "nn", "no", "oc", "pa", "pl", "ps", "pt",
            "ro", "ru", "sa", "sd", "si", "sk", "sl", "sn", "so", "sq", "sr", "su", "sv", "sw",
            "ta", "te", "tg", "th", "tk", "tl", "tr", "tt", "uk", "ur", "uz", "vi", "yi", "yo",
            "yue", "zh",
        ],
        recommended: false,
        english_error: Some(4.4),
        seconds_per_30s: Some(9.69),
    },
    SpeechModel {
        id: "whisper-medium",
        name: "Whisper Medium",
        description: "Broadest language support, but may run a bit slow",
        file: ModelFile {
            name: "whisper-medium-Q8_0.gguf",
            url: "https://huggingface.co/handy-computer/whisper-medium-gguf/resolve/ec78f06fded51aa82cde751678b78f76f78c8b7f/whisper-medium-Q8_0.gguf",
            sha256: "09e6a65e7de377aa5b10bae24608bc6f8ca2ed04b3993ef10d4a02bcd9a82adf",
            size: 831538144,
        },
        languages: WHISPER,
        recommended: true,
        english_error: Some(4.6),
        seconds_per_30s: Some(7.34),
    },
    SpeechModel {
        id: "parakeet-v3",
        name: "Parakeet TDT 0.6B v3",
        description: "Fast and accurate, great overall multilingual pick",
        file: ModelFile {
            name: "parakeet-tdt-0.6b-v3-Q8_0.gguf",
            url: "https://huggingface.co/handy-computer/parakeet-tdt-0.6b-v3-gguf/resolve/85ac09ea12fc4b1112fa76810059364bc6adc9de/parakeet-tdt-0.6b-v3-Q8_0.gguf",
            sha256: "5859f77944efcd8eafa23a6350731960b2b55b2203df51f319665c807d802cc7",
            size: 739508576,
        },
        languages: &[
            "bg", "hr", "cs", "da", "nl", "en", "et", "fi", "fr", "de", "el", "hu", "it", "lv",
            "lt", "mt", "pl", "pt", "ro", "ru", "sk", "sl", "es", "sv", "uk",
        ],
        recommended: false,
        english_error: Some(4.8),
        seconds_per_30s: Some(1.20),
    },
    SpeechModel {
        id: "qwen3-asr-0.6b",
        name: "Qwen3-ASR 0.6B",
        description: "Best for Chinese, Cantonese, and Asian languages with English",
        file: ModelFile {
            name: "Qwen3-ASR-0.6B-Q8_0.gguf",
            url: "https://huggingface.co/handy-computer/Qwen3-ASR-0.6B-gguf/resolve/e4e16599b900eb0cb36e524514756bb92eb092b7/Qwen3-ASR-0.6B-Q8_0.gguf",
            sha256: "f081b2d5e23bd669d92cc331d722a8a0681943b8e6f34b48996fd5c319b5acd8",
            size: 850423456,
        },
        languages: &[
            "zh", "en", "yue", "ar", "de", "fr", "es", "pt", "id", "it", "ko", "ru", "th", "vi",
            "ja", "tr", "hi", "ms", "nl", "sv", "da", "fi", "pl", "cs", "fil", "fa", "el", "ro",
            "hu", "mk",
        ],
        recommended: false,
        english_error: Some(4.2),
        seconds_per_30s: Some(3.09),
    },
    SpeechModel {
        id: "whisper-small",
        name: "Whisper Small",
        description: "Many languages in a small model",
        file: ModelFile {
            name: "whisper-small-Q8_0.gguf",
            url: "https://huggingface.co/handy-computer/whisper-small-gguf/resolve/c0214bd34be9296695486f838e0142f900803159/whisper-small-Q8_0.gguf",
            sha256: "9b9c8811bbcc82a7766f0fb0925614bdacb0923b2cc630daeac17108b655b860",
            size: 269751136,
        },
        languages: WHISPER,
        recommended: false,
        english_error: Some(6.5),
        seconds_per_30s: Some(2.57),
    },
    SpeechModel {
        id: "whisper-base",
        name: "Whisper Base",
        description: "Small, many languages, less accurate",
        file: ModelFile {
            name: "whisper-base-Q8_0.gguf",
            url: "https://huggingface.co/handy-computer/whisper-base-gguf/resolve/e0f69524f648720eca44c024d1d0dbb7027d1fa0/whisper-base-Q8_0.gguf",
            sha256: "81c069428bc8a24551a8169cf31cf09bcfd9d4cf50389ae281323c9aa9648c81",
            size: 84962880,
        },
        languages: WHISPER,
        recommended: false,
        english_error: Some(9.9),
        seconds_per_30s: Some(1.03),
    },
    SpeechModel {
        id: "moonshine-base",
        name: "Moonshine Base",
        description: "Very small and quick, English only",
        file: ModelFile {
            name: "moonshine-base-Q8_0.gguf",
            url: "https://huggingface.co/handy-computer/moonshine-base-gguf/resolve/3ef112378a8cf46ac8b278d9bfa2d15c846704b8/moonshine-base-Q8_0.gguf",
            sha256: "7f0027dfd857d310b63a85ef57cadf183da712cc374f85a648f8bc18aaa2efc8",
            size: 77476480,
        },
        languages: &["en"],
        recommended: false,
        english_error: Some(12.2),
        seconds_per_30s: Some(0.84),
    },
    SpeechModel {
        id: "moonshine-tiny",
        name: "Moonshine Tiny",
        description: "The smallest; quick but less accurate, English only",
        file: ModelFile {
            name: "moonshine-tiny-Q8_0.gguf",
            url: "https://huggingface.co/handy-computer/moonshine-tiny-gguf/resolve/f5c11906eba3f44cf305eed30feb9cbfb0b4b9d0/moonshine-tiny-Q8_0.gguf",
            sha256: "2fd348d7b38f97d309cc3ec6848f3f57f537b80244950f07d2637e463f95a3a1",
            size: 35466912,
        },
        languages: &["en"],
        recommended: false,
        english_error: Some(14.1),
        seconds_per_30s: Some(0.50),
    },
];

pub fn model(id: &str) -> Option<&'static SpeechModel> {
    MODELS.iter().find(|m| m.id == id)
}
