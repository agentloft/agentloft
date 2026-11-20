//! The models offered: small instruction-tuned ones from ungated Hugging Face
//! repositories, each file pinned to a commit and checked by its SHA-256.
//!
//! Tokenizers are saved under a name of their family's, not `tokenizer.json`,
//! so models can share one folder: the Qwen3 sizes share one tokenizer (same
//! hash), and Gemma's would otherwise overwrite it.
//!
//! Descriptions follow what the live test measured (`tests/live.rs`, its
//! prompt): only Gemma 3 4B reliably kept just the correction where the
//! speaker corrected themselves; the smaller ones fix punctuation, capitals
//! and filler words but often keep both versions. On an M-series Mac
//! (Metal) a short transcript took about 0.1 s with the 1B-and-under models
//! and 0.3 s with the 4B ones; on its CPU, 1–4 s.

use crate::{Arch, LlmModel, ModelFile};

const QWEN3_TOKENIZER_SHA256: &str =
    "aeb13307a71acd8fe81861d94ad54ab689df773318809eed3cbe794b4492dae4";
const QWEN3_TOKENIZER_SIZE: u64 = 11_422_654;
const GEMMA3_TOKENIZER_SHA256: &str =
    "4667f2089529e8e7657cfb6d1c19910ae71ff5f28aa7ab2ff2763330affad795";
const GEMMA3_TOKENIZER_SIZE: u64 = 33_384_568;

pub const MODELS: &[LlmModel] = &[
    LlmModel {
        id: "qwen3-0.6b",
        name: "Qwen3 0.6B",
        description: "Smallest and fastest; tidies punctuation but often misses corrections",
        arch: Arch::Qwen3,
        files: &[
            ModelFile {
                name: "Qwen3-0.6B-Q8_0.gguf",
                url: "https://huggingface.co/Qwen/Qwen3-0.6B-GGUF/resolve/23749fefcc72300e3a2ad315e1317431b06b590a/Qwen3-0.6B-Q8_0.gguf",
                sha256: "9465e63a22add5354d9bb4b99e90117043c7124007664907259bd16d043bb031",
                size: 639_446_688,
            },
            ModelFile {
                name: "qwen3-tokenizer.json",
                url: "https://huggingface.co/Qwen/Qwen3-0.6B/resolve/c1899de289a04d12100db370d81485cdf75e47ca/tokenizer.json",
                sha256: QWEN3_TOKENIZER_SHA256,
                size: QWEN3_TOKENIZER_SIZE,
            },
        ],
        recommended: false,
    },
    LlmModel {
        id: "qwen3-1.7b",
        name: "Qwen3 1.7B",
        description: "Small and fast; misses some spoken corrections",
        arch: Arch::Qwen3,
        files: &[
            ModelFile {
                name: "Qwen3-1.7B-Q4_K_M.gguf",
                url: "https://huggingface.co/unsloth/Qwen3-1.7B-GGUF/resolve/d7f544eead698dbd1f15126ef60b45a1e1933222/Qwen3-1.7B-Q4_K_M.gguf",
                sha256: "b139949c5bd74937ad8ed8c8cf3d9ffb1e99c866c823204dc42c0d91fa181897",
                size: 1_107_409_472,
            },
            ModelFile {
                name: "qwen3-tokenizer.json",
                url: "https://huggingface.co/Qwen/Qwen3-1.7B/resolve/70d244cc86ccca08cf5af4e1e306ecf908b1ad5e/tokenizer.json",
                sha256: QWEN3_TOKENIZER_SHA256,
                size: QWEN3_TOKENIZER_SIZE,
            },
        ],
        recommended: false,
    },
    LlmModel {
        id: "gemma3-1b",
        name: "Gemma 3 1B",
        description: "Google's small model; fast on a Mac, misses some spoken corrections",
        arch: Arch::Gemma3,
        files: &[
            ModelFile {
                name: "gemma-3-1b-it-Q4_K_M.gguf",
                url: "https://huggingface.co/unsloth/gemma-3-1b-it-GGUF/resolve/f0b45be0aac41bd6a100a4b5734cad5f67255bfb/gemma-3-1b-it-Q4_K_M.gguf",
                sha256: "8270790f3ab69fdfe860b7b64008d9a19986d8df7e407bb018184caa08798ebd",
                size: 806_058_272,
            },
            ModelFile {
                name: "gemma3-tokenizer.json",
                url: "https://huggingface.co/unsloth/gemma-3-1b-it/resolve/5b11413a10db4e486ef16a20101fd028f8f2499c/tokenizer.json",
                sha256: GEMMA3_TOKENIZER_SHA256,
                size: GEMMA3_TOKENIZER_SIZE,
            },
        ],
        recommended: false,
    },
    LlmModel {
        id: "qwen3-4b",
        name: "Qwen3 4B",
        description: "Good punctuation and numbers, misses some corrections; about 2.5 GB of memory",
        arch: Arch::Qwen3,
        files: &[
            ModelFile {
                name: "Qwen3-4B-Q4_K_M.gguf",
                url: "https://huggingface.co/unsloth/Qwen3-4B-GGUF/resolve/22c9fc8a8c7700b76a1789366280a6a5a1ad1120/Qwen3-4B-Q4_K_M.gguf",
                sha256: "f6f851777709861056efcdad3af01da38b31223a3ba26e61a4f8bf3a2195813a",
                size: 2_497_281_312,
            },
            ModelFile {
                name: "qwen3-tokenizer.json",
                url: "https://huggingface.co/Qwen/Qwen3-4B/resolve/1cfa9a7208912126459214e8b04321603b3df60c/tokenizer.json",
                sha256: QWEN3_TOKENIZER_SHA256,
                size: QWEN3_TOKENIZER_SIZE,
            },
        ],
        recommended: false,
    },
    LlmModel {
        id: "gemma3-4b",
        name: "Gemma 3 4B",
        description: "Best results, spoken corrections included; about 3 GB of memory",
        arch: Arch::Gemma3,
        files: &[
            ModelFile {
                name: "gemma-3-4b-it-Q4_K_M.gguf",
                url: "https://huggingface.co/unsloth/gemma-3-4b-it-GGUF/resolve/5a3566e716d80f709ed7b79817eaf7733d2a1fce/gemma-3-4b-it-Q4_K_M.gguf",
                sha256: "04a43a22e8d2003deda5acc262f68ec1005fa76c735a9962a8c77042a74a7d19",
                size: 2_489_894_016,
            },
            ModelFile {
                name: "gemma3-tokenizer.json",
                url: "https://huggingface.co/unsloth/gemma-3-4b-it/resolve/bf46152c47f5dd20b896357cb51abc4c03b8ee8c/tokenizer.json",
                sha256: GEMMA3_TOKENIZER_SHA256,
                size: GEMMA3_TOKENIZER_SIZE,
            },
        ],
        recommended: true,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalog_is_well_formed() {
        assert_eq!(MODELS.iter().filter(|m| m.recommended).count(), 1);
        for (i, m) in MODELS.iter().enumerate() {
            assert!(MODELS[..i].iter().all(|o| o.id != m.id), "{} twice", m.id);
            assert!(crate::model(m.id).is_some());
            assert_eq!(m.files.iter().filter(|f| is_gguf(f)).count(), 1, "{}", m.id);
            assert_eq!(m.files.len(), 2, "{}", m.id);
            for f in m.files {
                assert!(f.url.starts_with("https://huggingface.co/"), "{}", f.url);
                // Pinned to a commit, never a branch.
                let rev = f.url.split("/resolve/").nth(1).unwrap();
                let rev = rev.split('/').next().unwrap();
                assert_eq!(rev.len(), 40, "{}", f.url);
                assert!(rev.chars().all(|c| c.is_ascii_hexdigit()), "{}", f.url);
                assert_eq!(f.sha256.len(), 64, "{}", f.name);
                assert!(f.sha256.chars().all(|c| c.is_ascii_hexdigit()));
                assert!(f.size > 1_000_000, "{}", f.name);
                assert!(!f.name.contains('/'), "{}", f.name);
            }
        }
        // Files of the same name are the same file, so models can share a
        // folder.
        let all = MODELS.iter().flat_map(|m| m.files);
        for a in all.clone() {
            for b in all.clone() {
                if a.name == b.name {
                    assert_eq!((a.sha256, a.size), (b.sha256, b.size), "{}", a.name);
                }
            }
        }
        assert!(crate::model("nope").is_none());
    }

    fn is_gguf(f: &ModelFile) -> bool {
        f.name.ends_with(".gguf")
    }
}
