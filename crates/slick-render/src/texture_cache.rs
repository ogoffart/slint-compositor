//! Tracks the GL texture currently associated with each client surface.

use std::collections::HashMap;

use crate::Size;

/// A GL texture imported from a client buffer.
#[derive(Debug, Clone, Copy)]
pub struct ImportedTexture {
    /// Raw GL texture id (`GLuint`), valid in Slint's GL context.
    pub gl_id: u32,
    pub size: Size,
    /// Bumped every time the underlying buffer contents change, so the UI layer
    /// can decide whether it must rebuild its `slint::Image`.
    pub generation: u64,
}

/// Maps surface id -> the texture last imported for it.
///
/// This is just bookkeeping; it holds no GL state itself, so it is safe to
/// construct and unit-test without a GL context.
#[derive(Debug, Default)]
pub struct TextureCache {
    entries: HashMap<u64, ImportedTexture>,
}

impl TextureCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, surface_id: u64) -> Option<ImportedTexture> {
        self.entries.get(&surface_id).copied()
    }

    /// Record a freshly imported texture, returning the new generation counter.
    pub fn insert(&mut self, surface_id: u64, gl_id: u32, size: Size) -> u64 {
        let generation = self
            .entries
            .get(&surface_id)
            .map_or(0, |t| t.generation + 1);
        self.entries.insert(
            surface_id,
            ImportedTexture {
                gl_id,
                size,
                generation,
            },
        );
        generation
    }

    /// Forget a surface (e.g. on unmap). Returns the GL id the caller should
    /// delete, if any.
    pub fn remove(&mut self, surface_id: u64) -> Option<u32> {
        self.entries.remove(&surface_id).map(|t| t.gl_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_increments_on_reimport() {
        let mut cache = TextureCache::new();
        let size = Size {
            width: 4,
            height: 4,
        };
        assert_eq!(cache.insert(1, 10, size), 0);
        assert_eq!(cache.insert(1, 11, size), 1);
        assert_eq!(cache.get(1).unwrap().gl_id, 11);
        assert_eq!(cache.remove(1), Some(11));
        assert!(cache.get(1).is_none());
    }
}
