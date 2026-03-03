//! SentencePiece tokenizer wrapper (T-15).
//!
//! Wraps the [`sentencepiece`] crate to provide a [`Tokenizer`]-compatible
//! interface for models that ship a native `.model` (proto) file such as
//! LLaMA-1/2, Mistral, Gemma, T5, and many others.
//!
//! The underlying C library is loaded from the system installation
//! (`libsentencepiece`) via pkg-config — no cmake build from source is needed
//! as long as `libsentencepiece-dev` (or equivalent) is installed.

use super::traits::{
    Decoder, Encoder, Encoding, SpecialTokens, TokenIdType, Tokenizer as TokenizerTrait,
};
use anyhow::{Error, Result};
use sentencepiece::SentencePieceProcessor;

/// SentencePiece tokenizer that wraps a native `.model` file.
pub struct SentencePieceTokenizer {
    processor: SentencePieceProcessor,
    special_tokens: SpecialTokens,
}

impl std::fmt::Debug for SentencePieceTokenizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SentencePieceTokenizer")
            .field("vocab_size", &self.processor.len())
            .finish()
    }
}

impl SentencePieceTokenizer {
    /// Load a SentencePiece model from a `.model` file path.
    pub fn from_file(path: &str) -> Result<Self> {
        let processor = SentencePieceProcessor::open(path)
            .map_err(|e| Error::msg(format!("Failed to load SentencePiece model '{}': {}", path, e)))?;

        // Extract well-known special tokens from the vocabulary.
        let special_tokens = Self::extract_special_tokens(&processor);

        Ok(Self {
            processor,
            special_tokens,
        })
    }

    /// Build a `SpecialTokens` struct using well-known token strings and the
    /// processor's `piece_to_id` lookup.
    fn extract_special_tokens(proc: &SentencePieceProcessor) -> SpecialTokens {
        // Returns the token string if it exists in the vocabulary.
        let has = |token: &str| -> Option<String> {
            proc.piece_to_id(token)
                .ok()
                .flatten()
                .map(|_| token.to_string())
        };

        // Use the processor's native BOS/EOS/UNK/PAD ids when available.
        let bos = proc.bos_id().and_then(|id| {
            // Decode the single ID to get its string representation.
            proc.decode_piece_ids(&[id]).ok()
        });
        let eos = proc.eos_id().and_then(|id| {
            proc.decode_piece_ids(&[id]).ok()
        });
        let unk = {
            let id = proc.unk_id();
            proc.decode_piece_ids(&[id]).ok()
        };
        let pad = proc.pad_id().and_then(|id| {
            proc.decode_piece_ids(&[id]).ok()
        });

        SpecialTokens {
            bos_token: bos.or_else(|| has("<s>")).or_else(|| has("<bos>")),
            eos_token: eos.or_else(|| has("</s>")).or_else(|| has("<eos>")),
            unk_token: unk.or_else(|| has("<unk>")),
            sep_token: has("[SEP]"),
            pad_token: pad.or_else(|| has("<pad>")).or_else(|| has("[PAD]")),
            cls_token: has("[CLS]"),
            mask_token: has("[MASK]"),
            additional_special_tokens: vec![],
        }
    }
}

impl Encoder for SentencePieceTokenizer {
    fn encode(&self, input: &str) -> Result<Encoding> {
        let pieces = self
            .processor
            .encode(input)
            .map_err(|e| Error::msg(format!("SentencePiece encode error: {}", e)))?;

        let ids: Vec<TokenIdType> = pieces.iter().map(|p| p.id as TokenIdType).collect();
        Ok(Encoding::Sp(ids))
    }

    fn encode_batch(&self, inputs: &[&str]) -> Result<Vec<Encoding>> {
        inputs.iter().map(|s| self.encode(s)).collect()
    }
}

impl Decoder for SentencePieceTokenizer {
    fn decode(&self, token_ids: &[TokenIdType], _skip_special_tokens: bool) -> Result<String> {
        // SentencePieceProcessor::decode expects &[u32].
        let ids: Vec<u32> = token_ids.to_vec();
        self.processor
            .decode_piece_ids(&ids)
            .map_err(|e| Error::msg(format!("SentencePiece decode error: {}", e)))
    }
}

impl TokenizerTrait for SentencePieceTokenizer {
    fn vocab_size(&self) -> usize {
        self.processor.len()
    }

    fn get_special_tokens(&self) -> &SpecialTokens {
        &self.special_tokens
    }

    fn token_to_id(&self, token: &str) -> Option<TokenIdType> {
        self.processor
            .piece_to_id(token)
            .ok()
            .flatten()
            .map(|id| id as TokenIdType)
    }

    fn id_to_token(&self, id: TokenIdType) -> Option<String> {
        // Decode a single ID; for special tokens this returns the piece string.
        self.processor.decode_piece_ids(&[id]).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: returns `true` if there is a `.model` file available for tests.
    /// CI environments without a model file skip these tests gracefully.
    fn model_file() -> Option<String> {
        // Allow overriding via env var for CI / local testing
        if let Ok(path) = std::env::var("TEST_SENTENCEPIECE_MODEL") {
            if std::path::Path::new(&path).exists() {
                return Some(path);
            }
        }
        None
    }

    #[test]
    fn test_load_missing_file_returns_error() {
        let result = SentencePieceTokenizer::from_file("/nonexistent/path/model.model");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Failed to load"));
    }

    #[test]
    fn test_encode_decode_roundtrip() {
        let Some(path) = model_file() else {
            println!("Skipping: TEST_SENTENCEPIECE_MODEL not set");
            return;
        };
        let tok = SentencePieceTokenizer::from_file(&path).expect("load model");
        assert!(tok.vocab_size() > 0);

        let text = "Hello, world!";
        let enc = tok.encode(text).expect("encode");
        assert!(!enc.token_ids().is_empty());

        let decoded = tok.decode(enc.token_ids(), false).expect("decode");
        assert_eq!(decoded.trim(), text.trim());
    }

    #[test]
    fn test_vocab_size_positive() {
        let Some(path) = model_file() else {
            println!("Skipping: TEST_SENTENCEPIECE_MODEL not set");
            return;
        };
        let tok = SentencePieceTokenizer::from_file(&path).expect("load model");
        assert!(tok.vocab_size() > 100, "vocab should have >100 tokens");
    }

    #[test]
    fn test_encode_batch() {
        let Some(path) = model_file() else {
            println!("Skipping: TEST_SENTENCEPIECE_MODEL not set");
            return;
        };
        let tok = SentencePieceTokenizer::from_file(&path).expect("load model");
        let inputs = ["Hello", "World", "foo bar baz"];
        let results = tok.encode_batch(&inputs).expect("encode_batch");
        assert_eq!(results.len(), 3);
        for enc in &results {
            assert!(!enc.token_ids().is_empty());
        }
    }
}
