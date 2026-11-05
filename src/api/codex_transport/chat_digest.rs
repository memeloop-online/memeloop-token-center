/// Evidence for bytes already emitted downstream. This cannot reconstruct text;
/// only a real later upstream value can supply the remaining suffix.
#[derive(Default)]
pub(super) struct EmittedText {
    bytes: u64,
    digest: blake3::Hasher,
}

impl EmittedText {
    pub(super) fn from_text(text: &str) -> Self {
        let mut value = Self::default();
        value.bytes = text.len() as u64;
        value.digest.update(text.as_bytes());
        value
    }

    pub(super) fn observe(&mut self, text: &str) -> Result<(), &'static str> {
        self.bytes = self
            .bytes
            .checked_add(text.len() as u64)
            .ok_or("upstream_invalid_response")?;
        self.digest.update(text.as_bytes());
        Ok(())
    }

    pub(super) fn is_empty(&self) -> bool {
        self.bytes == 0
    }

    pub(super) fn matches(&self, text: &str) -> bool {
        self.bytes == text.len() as u64 && self.digest.finalize() == blake3::hash(text.as_bytes())
    }

    pub(super) fn remaining<'a>(&self, completed: &'a str) -> Option<&'a str> {
        let length = usize::try_from(self.bytes).ok()?;
        // str::get rejects a prefix ending inside a UTF-8 scalar as well as a
        // terminal shorter than the already-emitted prefix.
        let prefix = completed.get(..length)?;
        self.matches(prefix).then(|| &completed[length..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emitted_prefix_checks_order_length_and_utf8_boundaries() {
        let mut text = EmittedText::default();
        text.observe("你").unwrap();
        text.observe("好").unwrap();
        assert_eq!(text.remaining("你好 world"), Some(" world"));
        assert!(text.remaining("好你 world").is_none());
        assert!(text.remaining("你").is_none());
        let text = EmittedText::from_text("a");
        assert!(text.remaining("你").is_none());
        assert!(text.remaining("b").is_none());
        assert_eq!(EmittedText::default().remaining("你"), Some("你"));
    }
}
