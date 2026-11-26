use crate::models::{EmbedModel, normalize};
use anyhow::Result;

fn tokens(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
}

pub struct FakeEmbedder(usize);

impl FakeEmbedder {
    pub fn new(dim: usize) -> Self {
        Self(dim)
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        let mut vector = vec![0.0; self.0];
        for token in tokens(text) {
            let hash = blake3::hash(token.as_bytes());
            let prefix = u64::from_le_bytes(hash.as_bytes()[..8].try_into().unwrap());
            vector[(prefix % self.0 as u64) as usize] += 1.0;
        }
        normalize(&mut vector);
        vector
    }
}

impl EmbedModel for FakeEmbedder {
    fn model_id(&self) -> &str {
        "test-hash"
    }
    fn dim(&self) -> usize {
        self.0
    }
    fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        Ok(self.embed(query))
    }
    fn embed_docs(&self, docs: &[(&str, &str)]) -> Result<Vec<Vec<f32>>> {
        Ok(docs
            .iter()
            .map(|(header, text)| self.embed(&format!("{header} {text}")))
            .collect())
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_embedder_deterministic() {
        let model = FakeEmbedder::new(64);
        assert_eq!(model.embed("Miete Miete"), model.embed("miete"));
    }

}
